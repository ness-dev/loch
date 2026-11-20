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
use std::path::Path;

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

/// How many distinct substantive lines a session must have written, at the
/// same relative paths, into this repository's files before a directory that
/// no longer exists is taken for one of its former worktrees.
const EVIDENCE: usize = 5;

/// The repository a backfill covers, whichever of its worktrees it was run
/// from: every worktree git knows of, and what its branches hold, to
/// recognize the worktrees it no longer knows of by the text written in them.
struct Repo {
    root: String,
    /// `git worktree list`, the main one first, including any whose
    /// directory has gone but which git hasn't pruned yet.
    worktrees: Vec<String>,
    /// Path → every blob it has at the tip of any branch, tag or worktree.
    tree: HashMap<String, HashSet<String>>,
    /// Blob → the hashes of its lines, read on demand.
    blobs: HashMap<String, HashSet<String>>,
}

impl Repo {
    fn open(root: &str) -> Repo {
        let mut repo = Repo {
            root: root.to_string(),
            worktrees: vec![root.to_string()],
            tree: HashMap::new(),
            blobs: HashMap::new(),
        };
        if !snapshot::has_git_marker(root) {
            return repo;
        }
        let listing =
            git::git(root, &["worktree", "list", "--porcelain"], None).unwrap_or_default();
        let mut tips: Vec<String> = Vec::new();
        for line in listing.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                let path = sys::realpath_strict(path).unwrap_or_else(|| path.to_string());
                if !repo.worktrees.contains(&path) {
                    repo.worktrees.push(path);
                }
            } else if let Some(head) = line.strip_prefix("HEAD ") {
                tips.push(head.to_string());
            }
        }
        let refs = git::git(
            root,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ],
            None,
        )
        .unwrap_or_default();
        tips.extend(refs.lines().map(str::to_string));
        tips.sort();
        tips.dedup();
        // Walked a level at a time rather than `ls-tree -r` per tip: hundreds
        // of branches share nearly every subtree, and each (subtree, path) is
        // read once.
        let mut level: Vec<(String, String)> = tips
            .iter()
            .map(|tip| (format!("{}^{{tree}}", tip), String::new()))
            .collect();
        let width = match git::git(root, &["rev-parse", "--show-object-format"], None) {
            Ok(format) if format.trim() == "sha256" => 32,
            _ => 20,
        };
        let mut seen: HashSet<(String, String)> = HashSet::new();
        while !level.is_empty() {
            let names: Vec<&str> = level.iter().map(|(name, _)| name.as_str()).collect();
            let mut next = Vec::new();
            for ((_, prefix), object) in level.iter().zip(batch(root, &names)) {
                let Some((_, data)) = object else {
                    continue;
                };
                for (mode, name, id) in tree_entries(&data, width).unwrap_or_default() {
                    let path = format!("{}{}", prefix, name);
                    if mode == "40000" {
                        let sub = (id, format!("{}/", path));
                        if seen.insert(sub.clone()) {
                            next.push(sub);
                        }
                    } else if mode.starts_with("100") {
                        repo.tree.entry(path).or_default().insert(id);
                    }
                }
            }
            level = next;
        }
        repo
    }

    /// Reads every blob in `wanted` not yet read, in one `cat-file`.
    fn load(&mut self, wanted: &HashSet<String>) {
        let missing: Vec<&str> = wanted
            .iter()
            .filter(|b| !self.blobs.contains_key(*b))
            .map(String::as_str)
            .collect();
        for (blob, object) in missing.iter().zip(batch(&self.root, &missing)) {
            let lines = object
                .map(|(_, data)| {
                    String::from_utf8_lossy(&data)
                        .lines()
                        .map(attribution::line_hash)
                        .collect()
                })
                .unwrap_or_default();
            self.blobs.insert(blob.to_string(), lines);
        }
    }

    /// The live worktree `path` is in, the most deeply nested if several.
    fn worktree_holding(&self, path: &str) -> Option<&String> {
        self.worktrees
            .iter()
            .filter(|w| path == w.as_str() || inside(path, w).is_some())
            .max_by_key(|w| w.len())
    }
}

/// Each named object as `cat-file --batch` gives it, `(id, contents)`, in the
/// order asked; `None` for one that doesn't exist.
fn batch(root: &str, names: &[&str]) -> Vec<Option<(String, Vec<u8>)>> {
    if names.is_empty() {
        return Vec::new();
    }
    let input: String = names.iter().map(|n| format!("{}\n", n)).collect();
    let out = git::git_bytes(root, &["cat-file", "--batch"], Some(&input)).unwrap_or_default();
    let mut objects = Vec::with_capacity(names.len());
    let mut at = 0;
    while objects.len() < names.len() {
        let Some(end) = out[at..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&out[at..at + end]).into_owned();
        at += end + 1;
        let parts: Vec<&str> = header.split(' ').collect();
        match (
            parts.as_slice(),
            parts.get(2).and_then(|s| s.parse::<usize>().ok()),
        ) {
            ([id, _, _], Some(size)) => {
                let body = out[at..(at + size).min(out.len())].to_vec();
                at += size + 1;
                objects.push(Some((id.to_string(), body)));
            }
            _ => objects.push(None),
        }
    }
    objects.resize(names.len(), None);
    objects
}

/// `(mode, name, id)` for each entry of a raw tree object whose ids are
/// `width` bytes.
fn tree_entries(data: &[u8], width: usize) -> Option<Vec<(String, String, String)>> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let space = at + data[at..].iter().position(|&b| b == b' ')?;
        let nul = space + data[space..].iter().position(|&b| b == 0)?;
        let id = data.get(nul + 1..nul + 1 + width)?;
        let mode = std::str::from_utf8(&data[at..space]).ok()?;
        if mode.is_empty() || !mode.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        entries.push((
            mode.to_string(),
            String::from_utf8_lossy(&data[space + 1..nul]).into_owned(),
            id.iter().map(|b| format!("{:02x}", b)).collect(),
        ));
        at = nul + 1 + width;
    }
    Some(entries)
}

/// A line distinctive enough that finding it at the same path in two
/// unrelated repositories would be a coincidence.
fn substantive(line: &str) -> bool {
    !attribution::is_weak(line) && line.trim().chars().count() >= 12
}

/// What a session's working directory says about where it worked.
enum Place {
    /// A worktree of this repository: the directory its paths are relative to.
    Root(String),
    /// Somewhere that may have been one of its worktrees and isn't any more:
    /// the candidate roots, outermost first, and the live worktree it lay
    /// inside, if any, to fall back on.
    Gone(Vec<String>, Option<String>),
    /// Another repository's checkout.
    Elsewhere,
}

fn place(cwd: &str, repo: &Repo) -> Place {
    let found = snapshot::workspace_root(cwd);
    let checkout = snapshot::has_git_marker(&found).then_some(found);
    let live = repo.worktree_holding(cwd).cloned();
    if Path::new(cwd).is_dir() {
        if let Some(checkout) = &checkout {
            if repo.worktrees.contains(checkout) {
                return Place::Root(checkout.clone());
            }
        }
    }
    // A checkout above a directory that is gone is the one it was in — unless
    // it also encloses this repository, as a home directory under git does.
    let floor = match (&checkout, &live) {
        // A repository nested in a worktree (a submodule, a vendored clone)
        // is its own project.
        (Some(checkout), Some(live)) if inside(checkout, live).is_some() => {
            return Place::Elsewhere
        }
        (_, Some(live)) => Some(live.clone()),
        (Some(checkout), None) if inside(&repo.root, checkout).is_some() => Some(checkout.clone()),
        (Some(_), None) => return Place::Elsewhere,
        (None, None) => None,
    };
    let mut candidates: Vec<String> = Vec::new();
    for (i, c) in cwd.char_indices().skip(1) {
        if c == '/' {
            candidates.push(cwd[..i].to_string());
        }
    }
    candidates.push(cwd.to_string());
    candidates.retain(|c| match &floor {
        Some(floor) if live.as_ref() == Some(floor) => c == floor || inside(c, floor).is_some(),
        Some(floor) => inside(c, floor).is_some(),
        None => true,
    });
    Place::Gone(candidates, live)
}

/// The root a directory that no longer exists was a worktree of this
/// repository at, judged by the text the session wrote: the candidate under
/// which the most of it is in this repository's files at the same relative
/// paths. A tie, or too little, decides nothing.
fn recognize(candidates: &[String], turns: &[Turn], repo: &mut Repo) -> Option<String> {
    let edits: Vec<&(String, String)> = turns.iter().flat_map(|t| t.edits.iter()).collect();
    let mut wanted: HashSet<String> = HashSet::new();
    for candidate in candidates {
        for (file, _) in &edits {
            if let Some(blobs) = inside(file, candidate).and_then(|p| repo.tree.get(&p)) {
                wanted.extend(blobs.iter().cloned());
            }
        }
    }
    repo.load(&wanted);
    let mut scores: Vec<(usize, &String)> = Vec::new();
    for candidate in candidates {
        let mut found: HashSet<String> = HashSet::new();
        for (file, text) in &edits {
            let Some(blobs) = inside(file, candidate).and_then(|p| repo.tree.get(&p)) else {
                continue;
            };
            for line in text.lines().filter(|l| substantive(l)) {
                let hash = attribution::line_hash(line);
                if blobs
                    .iter()
                    .any(|b| repo.blobs.get(b).is_some_and(|l| l.contains(&hash)))
                {
                    found.insert(hash);
                }
            }
        }
        scores.push((found.len(), candidate));
    }
    scores.sort_by(|a, b| b.0.cmp(&a.0));
    match scores.as_slice() {
        [(best, root), rest @ ..]
            if *best >= EVIDENCE && rest.first().is_none_or(|r| r.0 < *best) =>
        {
            Some((*root).clone())
        }
        _ => None,
    }
}

/// Whether a session in directories that are gone edited any path this
/// repository has under one of the candidate roots — read off the raw text,
/// since most such sessions belong to other repositories and parsing every
/// one of them whole is most of what backfill would cost. A path written
/// with an escape is taken as a possible match, so this only ever skips.
fn could_be_recognized(transcript: &str, places: &[Place], repo: &Repo) -> bool {
    let Ok(raw) = fs::read_to_string(transcript) else {
        return true;
    };
    let candidates: Vec<&String> = places
        .iter()
        .flat_map(|p| match p {
            Place::Gone(candidates, _) => candidates.iter().collect(),
            _ => Vec::new(),
        })
        .collect();
    let key = "\"file_path\":";
    let mut rest = raw.as_str();
    while let Some(at) = rest.find(key) {
        rest = rest[at + key.len()..].trim_start_matches(' ');
        let Some(value) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = value.find('"') else {
            break;
        };
        let file = &value[..end];
        if file.contains('\\') {
            return true;
        }
        let known = candidates
            .iter()
            .any(|root| inside(file, root).is_some_and(|p| repo.tree.contains_key(&p)));
        if known {
            return true;
        }
        rest = &value[end..];
    }
    false
}

/// A session that worked in this repository, and the worktrees it worked in.
struct Session {
    id: String,
    /// The worktree its records are filed under: where it ended up.
    root: String,
    roots: Vec<String>,
    turns: Vec<Turn>,
}

/// The transcripts that hold this repository's history: every local session
/// whose working directory was one of its worktrees — the checkout `root`,
/// any other git still lists, or one since removed, which is recognized by
/// what the session wrote there rather than by where it was. Worktrees live
/// wherever their creator put them, so no layout is assumed.
fn sessions_for(repo: &mut Repo) -> Vec<Session> {
    let mut found = Vec::new();
    let projects = claude::projects_dir(None);
    let Ok(dirs) = fs::read_dir(&projects) else {
        return found;
    };
    let mut paths: Vec<String> = Vec::new();
    for dir in dirs.flatten() {
        let Ok(files) = fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path().to_string_lossy().into_owned();
            if path.ends_with(".jsonl") {
                paths.push(path);
            }
        }
    }
    for path in paths {
        let Some(meta) = claude::read_session_meta(&path) else {
            continue;
        };
        let places: Vec<Place> = std::iter::once(&meta.cwd)
            .chain(&meta.other_cwds)
            .map(|cwd| place(cwd, repo))
            .collect();
        if places.iter().all(|p| matches!(p, Place::Elsewhere)) {
            continue;
        }
        let only_gone = places
            .iter()
            .all(|p| matches!(p, Place::Elsewhere | Place::Gone(_, None)));
        if only_gone && !could_be_recognized(&path, &places, repo) {
            continue;
        }
        let turns = turns_of(&path);
        let mut roots: Vec<String> = Vec::new();
        for place in places {
            let root = match place {
                Place::Root(root) => Some(root),
                Place::Gone(candidates, live) => recognize(&candidates, &turns, repo).or(live),
                Place::Elsewhere => None,
            };
            if let Some(root) = root {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        if roots.is_empty() {
            continue;
        }
        found.push(Session {
            id: meta.session_id,
            root: roots[0].clone(),
            roots,
            turns,
        });
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// `file` relative to the worktree it was written in: the most deeply nested
/// of this session's and the repository's that holds it.
fn relative_to(file: &str, roots: &[String], repo: &Repo) -> Option<String> {
    roots
        .iter()
        .chain(&repo.worktrees)
        .filter(|root| inside(file, root).is_some())
        .max_by_key(|root| root.len())
        .and_then(|root| inside(file, root))
}

#[derive(Default)]
pub struct Backfilled {
    pub session: String,
    /// The worktree the session worked in, which may no longer exist.
    pub root: String,
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
    let mut repo = Repo::open(&root);
    let found = sessions_for(&mut repo);
    let mut sessions: Vec<(String, Vec<Turn>)> = Vec::new();
    let mut places: Vec<(String, Vec<String>)> = Vec::new();
    for s in found {
        sessions.push((s.id, s.turns));
        places.push((s.root, s.roots));
    }
    let credited = credit(&sessions, &history, &notes);

    let mut results = Vec::new();
    for ((session, turns), (session_root, roots)) in sessions.into_iter().zip(places) {
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
            root: session_root.clone(),
            ..Backfilled::default()
        };
        for turn in turns {
            let mut files: Vec<FileChange> = Vec::new();
            for (file, text) in &turn.edits {
                let Some(relative) = relative_to(file, &roots, &repo) else {
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
            root: session_root.clone(),
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
