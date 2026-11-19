//! Which turn wrote which lines, recorded without git and found again by
//! content.
//!
//! At the start and end of every turn the workspace is snapshotted
//! (`snapshot.rs`), and the lines the turn added are what differs between the
//! two. However the agent made the change — an edit tool, `sed`, a generator
//! it ran, a formatter — it lands on disk the same way, which is the point:
//! this sees all of them and needs nothing from the harness but two moments.
//!
//! Each added line is recorded as a hash of its trimmed text, grouped into the
//! hunks the diff found, in one **edit record** per session in the store.
//! Hashes, not text: the record says which turn wrote a line without carrying
//! the code. Blame finds a line's turn by hashing the file in front of it and
//! matching runs of lines against those hunks, so the link survives anything
//! that keeps the text — a squash, a rebase, a cherry-pick, another clone —
//! and is lost by exactly what should lose it: someone rewriting the line.

use crate::index;
use crate::json::{self, Value};
use crate::object;
use crate::snapshot::{self, SnapshotError};
use crate::store::{Store, StoreError};
use crate::sys;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;

pub const RECORD_VERSION: i64 = 1;

/// The index dimension edit records are filed under, by `<project>:<path>`.
pub const FILE: &str = "file";

pub fn edits_key(agent: &str, session: &str) -> String {
    format!("edits/{}/{}.json", agent, session)
}

pub const EDITS_PREFIX: &str = "edits/";

/// A line's identity: its text without surrounding whitespace, so a formatter
/// re-indenting the agent's code doesn't sever the link.
pub fn line_hash(line: &str) -> String {
    let digest = Sha256::digest(line.trim().as_bytes());
    digest[..8].iter().map(|b| format!("{:02x}", b)).collect()
}

/// A line that says too little to be evidence on its own — a brace, a blank,
/// `end` — which every file has dozens of. It can join a run that something
/// distinctive anchors, never start one.
pub fn is_weak(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().filter(|c| c.is_alphanumeric()).count() < 3
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    /// 1-based line in the file as the turn left it.
    pub start: usize,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub hunks: Vec<Hunk>,
}

/// Text, if a blob is text: UTF-8 with no NUL in it.
fn text_of(data: &[u8]) -> Option<String> {
    if data.contains(&0) {
        return None;
    }
    String::from_utf8(data.to_vec()).ok()
}

/// The lines each changed file gained between two snapshots. A file that is
/// new counts as gaining all of its lines; a deleted or binary file has none
/// to attribute.
pub fn diff(before: Option<&str>, after: &str) -> Result<Vec<FileChange>, SnapshotError> {
    let old: HashMap<String, String> = match before {
        Some(id) => snapshot::read_snapshot(id)?
            .into_iter()
            .map(|e| (e.path, e.hash))
            .collect(),
        None => HashMap::new(),
    };
    let mut changes = Vec::new();
    for entry in snapshot::read_snapshot(after)? {
        let previous = old.get(&entry.path);
        if previous == Some(&entry.hash) {
            continue;
        }
        let Some(new_text) = text_of(&snapshot::read_object(&entry.hash)?) else {
            continue;
        };
        let old_text = match previous {
            Some(hash) => text_of(&snapshot::read_object(hash)?).unwrap_or_default(),
            None => String::new(),
        };
        let hunks = added_hunks(&old_text, &new_text);
        if !hunks.is_empty() {
            changes.push(FileChange {
                path: entry.path,
                hunks,
            });
        }
    }
    Ok(changes)
}

/// The runs of lines `new` has that `old` doesn't, by line diff.
pub fn added_hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut hunks: Vec<Hunk> = Vec::new();
    for op in diff.ops() {
        let range = op.new_range();
        let inserted = matches!(
            op.tag(),
            similar::DiffTag::Insert | similar::DiffTag::Replace
        );
        if !inserted || range.is_empty() {
            continue;
        }
        let lines: Vec<String> = new
            .lines()
            .skip(range.start)
            .take(range.len())
            .map(line_hash)
            .collect();
        hunks.push(Hunk {
            start: range.start + 1,
            lines,
        });
    }
    hunks
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnRecord {
    /// The transcript record of the prompt that began the turn, and the last
    /// record when it ended: where `ask` cuts a fork.
    pub prompt_uuid: Option<String>,
    pub end_uuid: Option<String>,
    pub started: Option<String>,
    pub ended: String,
    /// The first 120 characters of the prompt, redacted — the same one field
    /// of transcript content `list` carries, so blame can say what was asked
    /// without fetching the transcript.
    pub prompt: Option<String>,
    /// The turn was cut short — cancelled, or the session crashed — and was
    /// closed by the next prompt or the session's end rather than by `Stop`,
    /// so it ends where the workspace stood then.
    pub interrupted: bool,
    pub files: Vec<FileChange>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditRecord {
    pub agent: String,
    pub session: String,
    pub project: String,
    pub root: String,
    pub turns: Vec<TurnRecord>,
}

fn opt(value: &Option<String>) -> Value {
    Value::opt_str(value.as_deref())
}

impl EditRecord {
    pub fn to_json(&self) -> Value {
        let turns: Vec<Value> = self
            .turns
            .iter()
            .map(|turn| {
                let files: Vec<Value> = turn
                    .files
                    .iter()
                    .map(|file| {
                        let hunks: Vec<Value> = file
                            .hunks
                            .iter()
                            .map(|hunk| {
                                Value::Object(object! {
                                    "start" => hunk.start,
                                    "lines" => hunk.lines.iter().map(|l| Value::str(l.as_str())).collect::<Vec<_>>(),
                                })
                            })
                            .collect();
                        Value::Object(object! {"path" => file.path.as_str(), "hunks" => hunks})
                    })
                    .collect();
                Value::Object(object! {
                    "promptUuid" => opt(&turn.prompt_uuid),
                    "endUuid" => opt(&turn.end_uuid),
                    "started" => opt(&turn.started),
                    "ended" => turn.ended.as_str(),
                    "prompt" => opt(&turn.prompt),
                    "interrupted" => turn.interrupted,
                    "files" => files,
                })
            })
            .collect();
        Value::Object(object! {
            "loch" => RECORD_VERSION,
            "agent" => self.agent.as_str(),
            "session" => self.session.as_str(),
            "project" => self.project.as_str(),
            "root" => self.root.as_str(),
            "turns" => turns,
        })
    }

    pub fn from_json(value: &Value) -> Option<EditRecord> {
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        if value.get("loch").and_then(Value::as_i64) != Some(RECORD_VERSION) {
            return None;
        }
        let mut turns = Vec::new();
        for turn in value.get("turns")?.as_array()? {
            let mut files = Vec::new();
            for file in turn.get("files")?.as_array()? {
                let mut hunks = Vec::new();
                for hunk in file.get("hunks")?.as_array()? {
                    hunks.push(Hunk {
                        start: hunk.get("start")?.as_i64()? as usize,
                        lines: hunk
                            .get("lines")?
                            .as_array()?
                            .iter()
                            .filter_map(|l| l.as_str().map(str::to_string))
                            .collect(),
                    });
                }
                files.push(FileChange {
                    path: text(file, "path")?,
                    hunks,
                });
            }
            turns.push(TurnRecord {
                prompt_uuid: text(turn, "promptUuid"),
                end_uuid: text(turn, "endUuid"),
                started: text(turn, "started"),
                ended: text(turn, "ended")?,
                prompt: text(turn, "prompt"),
                interrupted: turn.get("interrupted").is_some_and(Value::truthy),
                files,
            });
        }
        Some(EditRecord {
            agent: text(value, "agent")?,
            session: text(value, "session")?,
            project: text(value, "project")?,
            root: text(value, "root").unwrap_or_default(),
            turns,
        })
    }

    /// Every path any turn changed, each once.
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .turns
            .iter()
            .flat_map(|t| t.files.iter().map(|f| f.path.clone()))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// Which project a workspace belongs to, the same on every clone and
/// worktree of it, so a teammate's blame finds your turns. For a git
/// repository that is its root commit — read once, nothing written, and
/// immune to anything later history does. Anything else is known by its
/// directory's name, which is weaker and the best available.
pub fn project_id(root: &str) -> String {
    if snapshot::has_git_marker(root) {
        if let Ok(out) = crate::git::git(root, &["rev-list", "--max-parents=0", "HEAD"], None) {
            let mut roots: Vec<&str> = out
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            roots.sort();
            if let Some(first) = roots.first() {
                return format!("git:{}", first);
            }
        }
    }
    format!("dir:{}", sys::basename(root))
}

pub fn file_value(project: &str, path: &str) -> String {
    format!("{}:{}", project, path)
}

// -- local copies ------------------------------------------------------------------

fn local_path(agent: &str, session: &str) -> std::path::PathBuf {
    snapshot::loch_home()
        .join("edits")
        .join(agent)
        .join(format!("{}.json", session))
}

/// The record as this machine last wrote it. The store gets a copy at every
/// turn's end; this one is what survives a store that didn't answer, and the
/// next turn's upload carries everything since.
pub fn load_local(agent: &str, session: &str) -> Option<EditRecord> {
    let text = fs::read_to_string(local_path(agent, session)).ok()?;
    EditRecord::from_json(&json::loads(&text).ok()?)
}

pub fn save_local(record: &EditRecord) -> std::io::Result<()> {
    let path = local_path(&record.agent, &record.session);
    snapshot::write_atomically(
        &path,
        json::dumps(&record.to_json(), json::COMPACT_UNICODE).as_bytes(),
    )
}

fn local_records() -> Vec<EditRecord> {
    let mut records = Vec::new();
    let Ok(agents) = fs::read_dir(snapshot::loch_home().join("edits")) else {
        return records;
    };
    for agent in agents.flatten() {
        let Ok(files) = fs::read_dir(agent.path()) else {
            continue;
        };
        for file in files.flatten() {
            if let Some(record) = fs::read_to_string(file.path())
                .ok()
                .and_then(|t| json::loads(&t).ok())
                .and_then(|v| EditRecord::from_json(&v))
            {
                records.push(record);
            }
        }
    }
    records
}

// -- the store -----------------------------------------------------------------------

/// The record first, then an index entry per file it touched — the same
/// order `save` keeps, for the same reason: an entry is derived from the
/// record, so a failure part-way leaves a record `reindex` can re-derive from
/// rather than an entry pointing at nothing.
pub fn publish(store: &dyn Store, record: &EditRecord) -> Result<(), StoreError> {
    let body = json::dumps(&record.to_json(), json::COMPACT_UNICODE);
    store.put(&edits_key(&record.agent, &record.session), body.as_bytes())?;
    for path in record.paths() {
        let key = index::index_key(
            FILE,
            Some(&file_value(&record.project, &path)),
            &record.session,
        );
        let entry =
            object! {"agent" => record.agent.as_str(), "session" => record.session.as_str()};
        store.put(
            &key,
            json::dumps(&Value::Object(entry), json::DEFAULT).as_bytes(),
        )?;
    }
    Ok(())
}

/// Every edit record that changed `path` in `project`: the store's, by its
/// file index, and this machine's own, which may hold turns the store hasn't
/// been sent yet. Where both have a session, the one with more turns wins.
pub fn records_for(store: Option<&dyn Store>, project: &str, path: &str) -> Vec<EditRecord> {
    let mut by_session: HashMap<String, EditRecord> = HashMap::new();
    let mut keep = |record: EditRecord| {
        let better = by_session
            .get(&record.session)
            .is_none_or(|have| record.turns.len() > have.turns.len());
        if better {
            by_session.insert(record.session.clone(), record);
        }
    };
    if let Some(store) = store {
        let prefix = index::value_prefix(FILE, Some(&file_value(project, path)));
        for key in store.list(&prefix).unwrap_or_default() {
            let Some(entry) = store
                .get(&key)
                .ok()
                .and_then(|data| String::from_utf8(data).ok())
                .and_then(|text| json::loads(&text).ok())
            else {
                continue;
            };
            let (Some(agent), Some(session)) = (
                entry.get("agent").and_then(Value::as_str),
                entry.get("session").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(record) = store
                .get(&edits_key(agent, session))
                .ok()
                .and_then(|data| String::from_utf8(data).ok())
                .and_then(|text| json::loads(&text).ok())
                .and_then(|v| EditRecord::from_json(&v))
            {
                keep(record);
            }
        }
    }
    for record in local_records() {
        if record.project == project && record.paths().iter().any(|p| p == path) {
            keep(record);
        }
    }
    let mut records: Vec<EditRecord> = by_session.into_values().collect();
    records.sort_by(|a, b| a.session.cmp(&b.session));
    records
}

/// Re-derives the file index from the records, for `reindex`. Returns the
/// keys written.
pub fn reindex_files(store: &dyn Store) -> Result<Vec<String>, StoreError> {
    let mut written = Vec::new();
    for key in store.list(EDITS_PREFIX)? {
        let Some(record) = String::from_utf8(store.get(&key)?)
            .ok()
            .and_then(|text| json::loads(&text).ok())
            .and_then(|v| EditRecord::from_json(&v))
        else {
            continue;
        };
        publish(store, &record)?;
        for path in record.paths() {
            written.push(index::index_key(
                FILE,
                Some(&file_value(&record.project, &path)),
                &record.session,
            ));
        }
    }
    Ok(written)
}

// -- blame -------------------------------------------------------------------------------

/// A turn, as blame points at one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TurnRef {
    pub record: usize,
    pub turn: usize,
}

/// For each line of `text`, the turn that most recently wrote it, if any.
///
/// A turn's hunk claims a run of the file's lines when the run's hashes equal
/// a run of the hunk's, consecutively, and at least one of them is not weak.
/// Runs rather than single lines, because one line of `return None` proves
/// nothing and five consecutive lines a turn wrote together prove a lot.
/// Turns are applied oldest first, so a later turn that rewrote the same text
/// takes it: blame answers "who last wrote this", as git's does.
pub fn attribute(text: &str, path: &str, records: &[EditRecord]) -> Vec<Option<TurnRef>> {
    let lines: Vec<&str> = text.lines().collect();
    let hashes: Vec<String> = lines.iter().map(|l| line_hash(l)).collect();
    let weak: Vec<bool> = lines.iter().map(|l| is_weak(l)).collect();
    let mut at: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, hash) in hashes.iter().enumerate() {
        at.entry(hash.as_str()).or_default().push(i);
    }

    let mut turns: Vec<(TurnRef, &TurnRecord)> = records
        .iter()
        .enumerate()
        .flat_map(|(r, record)| {
            record
                .turns
                .iter()
                .enumerate()
                .map(move |(t, turn)| (TurnRef { record: r, turn: t }, turn))
        })
        .collect();
    turns.sort_by(|a, b| a.1.ended.cmp(&b.1.ended));

    let mut owner: Vec<Option<TurnRef>> = vec![None; lines.len()];
    for (reference, turn) in turns {
        for file in turn.files.iter().filter(|f| f.path == path) {
            for hunk in &file.hunks {
                for (j, hash) in hunk.lines.iter().enumerate() {
                    for &i in at.get(hash.as_str()).map(Vec::as_slice).unwrap_or(&[]) {
                        // Only from the start of a run: the line before
                        // matching too means an earlier start covers it.
                        let continues = i > 0 && j > 0 && hashes[i - 1] == hunk.lines[j - 1];
                        if continues {
                            continue;
                        }
                        let mut length = 0;
                        while i + length < hashes.len()
                            && j + length < hunk.lines.len()
                            && hashes[i + length] == hunk.lines[j + length]
                        {
                            length += 1;
                        }
                        if (i..i + length).any(|k| !weak[k]) {
                            for slot in &mut owner[i..i + length] {
                                *slot = Some(reference);
                            }
                        }
                    }
                }
            }
        }
    }
    owner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_lines_are_what_the_turn_wrote() {
        let hunks = added_hunks("a\nb\nc\n", "a\nB\nb\nc\nd\n");
        assert_eq!(
            hunks,
            vec![
                Hunk {
                    start: 2,
                    lines: vec![line_hash("B")]
                },
                Hunk {
                    start: 5,
                    lines: vec![line_hash("d")]
                },
            ]
        );
    }

    fn record(turns: Vec<(&str, Vec<&str>)>) -> EditRecord {
        EditRecord {
            agent: "claude-code".into(),
            session: "s".into(),
            project: "p".into(),
            root: "/r".into(),
            turns: turns
                .into_iter()
                .map(|(ended, lines)| TurnRecord {
                    prompt_uuid: None,
                    end_uuid: None,
                    started: None,
                    ended: ended.into(),
                    prompt: None,
                    interrupted: false,
                    files: vec![FileChange {
                        path: "f.py".into(),
                        hunks: vec![Hunk {
                            start: 1,
                            lines: lines.into_iter().map(line_hash).collect(),
                        }],
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn a_lone_weak_line_is_not_evidence_but_joins_a_run() {
        let records = vec![record(vec![("1", vec!["def go():", "    return 1", "}"])])];
        let owners = attribute("}\nx = 1\ndef go():\n    return 1\n}\n", "f.py", &records);
        let claimed: Vec<bool> = owners.iter().map(Option::is_some).collect();
        assert_eq!(claimed, vec![false, false, true, true, true]);
    }

    #[test]
    fn the_latest_writer_of_identical_text_wins() {
        let records = vec![record(vec![
            ("2", vec!["value = compute()"]),
            ("1", vec!["value = compute()"]),
        ])];
        let owners = attribute("value = compute()\n", "f.py", &records);
        assert_eq!(owners, vec![Some(TurnRef { record: 0, turn: 0 })]);
    }

    #[test]
    fn reindentation_keeps_the_link() {
        let records = vec![record(vec![("1", vec!["return compute(x)"])])];
        assert!(attribute("        return compute(x)\n", "f.py", &records)[0].is_some());
    }
}
