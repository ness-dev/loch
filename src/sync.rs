//! `push` and `pull`: moving what this machine recorded to a remote, and what
//! a remote holds to this machine, as git's push and fetch do. Nothing else
//! reaches a remote for attribution; the hooks write the local store alone.
//!
//! **No manifest on the remote.** What a push has sent is remembered on this
//! machine (`~/.loch/pushed/<uri hash>.json`, key → hash of the local bytes),
//! so the remote stays a set of independent objects, each written whole by one
//! PUT. Bundles and index entries commute, as `save`s always have. Edit records
//! don't, being one mutable object per session, so they are merged by turn on
//! every push, pull and hook rather than replaced.
//!
//! **A delete on a remote stays deleted.** `delete --remote` leaves a
//! tombstone there (`tombstones/bundle/<ref>`, `tombstones/session/<agent>/
//! <id>`), one new key per delete, holding no content. Every push reads them
//! first and sends nothing they cover, so a teammate's copy can't bring a
//! deleted session back, and reads them again afterwards to withdraw what a
//! delete racing it covered. Every pull copies them here and forgets what
//! they cover. Being able to really delete is why loch uses object storage,
//! so this is stricter than git on purpose.

use crate::agent;
use crate::attribution::{self, EditRecord, EDITS_PREFIX, FILE};
use crate::index::{self, BRANCH, BUNDLES_PREFIX};
use crate::json::{self, Map, Value};
use crate::object;
use crate::redact;
use crate::snapshot;
use crate::store::{self, FileStore, Store, StoreError};
use crate::sys;
use crate::trace;
use indexmap::IndexMap;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const TOMBSTONES_PREFIX: &str = "tombstones/";
const TRANSCRIPTS_PREFIX: &str = "transcripts/";

pub fn bundle_tombstone(ref_: &str) -> String {
    format!("{}bundle/{}", TOMBSTONES_PREFIX, ref_)
}

pub fn session_tombstone(agent: &str, session: &str) -> String {
    format!("{}session/{}/{}", TOMBSTONES_PREFIX, agent, session)
}

/// A tombstone says when and from where, and nothing about what it buried.
pub fn tombstone_body() -> Vec<u8> {
    let body = object! {
        "deletedAt" => sys::iso_timestamp(sys::now()),
        "host" => sys::hostname(),
    };
    json::dumps(&Value::Object(body), json::DEFAULT).into_bytes()
}

#[derive(Clone, Debug, PartialEq)]
enum Owner {
    Bundle(String),
    Session(String),
}

/// What the tombstones cover. Sessions are matched by id alone: ids are
/// UUIDs, and a file-index key names no agent.
#[derive(Default)]
struct Dead {
    refs: BTreeSet<String>,
    sessions: BTreeSet<(String, String)>,
}

impl Dead {
    fn from_keys(keys: &[String]) -> Dead {
        let mut dead = Dead::default();
        for key in keys {
            let Some(rest) = key.strip_prefix(TOMBSTONES_PREFIX) else {
                continue;
            };
            let parts: Vec<&str> = rest.split('/').collect();
            match parts.as_slice() {
                ["bundle", ref_] => {
                    dead.refs.insert(ref_.to_string());
                }
                ["session", agent, session] => {
                    dead.sessions
                        .insert((agent.to_string(), session.to_string()));
                }
                _ => {}
            }
        }
        dead
    }

    fn covers(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Bundle(ref_) => self.refs.contains(ref_),
            Owner::Session(session) => self.sessions.iter().any(|(_, s)| s == session),
        }
    }
}

#[derive(Debug)]
pub enum SyncError {
    Store(StoreError),
    /// A secret survived redaction in a transcript; it was not sent.
    Redaction(String),
}

impl From<StoreError> for SyncError {
    fn from(error: StoreError) -> Self {
        SyncError::Store(error)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bundle,
    BranchEntry,
    Record,
    FileEntry,
    Transcript,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Bundle => "bundles",
            Kind::BranchEntry => "branchEntries",
            Kind::Record => "records",
            Kind::FileEntry => "fileEntries",
            Kind::Transcript => "transcripts",
        }
    }
}

const KINDS: [Kind; 5] = [
    Kind::Bundle,
    Kind::BranchEntry,
    Kind::Record,
    Kind::FileEntry,
    Kind::Transcript,
];

fn counts_payload(counts: &BTreeMap<&'static str, usize>) -> Value {
    let mut map = Map::new();
    for kind in KINDS {
        map.insert(
            kind.name().into(),
            Value::int(*counts.get(kind.name()).unwrap_or(&0) as i64),
        );
    }
    Value::Object(map)
}

/// What a push sends.
#[derive(Clone, Copy)]
pub enum Scope<'a> {
    /// This project's bundles and sessions.
    Project(&'a str),
    /// Every bundle, and every session.
    All,
    /// One bundle and its entries, for `save --push`.
    Bundle(&'a str),
}

struct Item {
    key: String,
    kind: Kind,
    owner: Owner,
    /// Bytes to send that the local store doesn't hold as such.
    data: Option<Vec<u8>>,
}

/// `edits/<agent>/<session>.json`, for an agent this build knows and an id
/// that can be a file name.
fn parse_edits_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix(EDITS_PREFIX)?;
    let (agent, file) = rest.split_once('/')?;
    let session = file.strip_suffix(".json")?;
    (agent::by_id(agent).is_some() && agent::is_safe_session_id(session))
        .then(|| (agent.to_string(), session.to_string()))
}

fn parse_record(data: &[u8]) -> Option<EditRecord> {
    String::from_utf8(data.to_vec())
        .ok()
        .and_then(|text| json::loads(&text).ok())
        .and_then(|v| EditRecord::from_json(&v))
}

/// Whether a stored transcript key is the session's. Both adapters' keys end
/// with the id and `.jsonl`, after a `/` (Claude) or a `-` (Codex's rollout
/// names).
fn transcript_of(key: &str, agent: &str, session: &str) -> bool {
    let Some(rest) = key.strip_prefix(&trace::transcripts_prefix(agent)) else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".jsonl") else {
        return false;
    };
    let Some(before) = stem.strip_suffix(session) else {
        return false;
    };
    // No segment may start with `.`: `find_transcript` would skip it, and it
    // is how a staged write is named.
    let segments_ok = rest.split('/').all(|seg| !seg.starts_with('.'));
    segments_ok && (before.is_empty() || before.ends_with('/') || before.ends_with('-'))
}

/// The project a file-index key's value belongs to is the part before the
/// path; values are `<project>:<path>`, and project ids hold one colon.
fn file_index_session(key: &str, project: &str) -> Option<String> {
    let rest = key.strip_prefix(&index::dimension_prefix(FILE))?;
    let (value, session) = rest.split_once('/')?;
    index::unquote(value)
        .starts_with(&format!("{}:", project))
        .then(|| session.to_string())
}

/// This machine's sessions in the local store, with their records.
fn local_sessions(local: &dyn Store) -> Result<Vec<(String, String, EditRecord)>, StoreError> {
    let mut sessions = Vec::new();
    for key in local.list(EDITS_PREFIX)? {
        let Some((agent, session)) = parse_edits_key(&key) else {
            continue;
        };
        let Some(record) = local.get(&key).ok().and_then(|d| parse_record(&d)) else {
            continue;
        };
        sessions.push((agent, session, record));
    }
    Ok(sessions)
}

/// Whether any session a bundle holds ran in `project`. A project-scoped push
/// must not send a bundle saved in another repository: that would publish one
/// client's transcripts to another's remote. A session whose directory is gone
/// from this machine can't be placed, and is only sent by `--all`.
fn bundle_in_project(
    local: &dyn Store,
    key: &str,
    project: &str,
    projects: &mut HashMap<String, String>,
) -> Result<bool, StoreError> {
    let packed = local.get(key)?;
    let Ok(bundle) = crate::bundle::unpack_bundle(&packed) else {
        return Ok(false);
    };
    for session in &bundle.sessions {
        let id = projects.entry(session.cwd.clone()).or_insert_with(|| {
            if std::path::Path::new(&session.cwd).is_dir() {
                attribution::project_id(&snapshot::workspace_root(&session.cwd))
            } else {
                String::new()
            }
        });
        if id == project {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Everything a push would send, in the order it has to arrive: a bundle
/// before its index entries and a record before its file index, as `save` and
/// the hooks have always written them, so a push cut short leaves objects
/// `reindex` can re-derive entries from rather than entries naming nothing.
fn plan(local: &dyn Store, scope: Scope) -> Result<Vec<Item>, StoreError> {
    let mut items = Vec::new();
    let mut projects = HashMap::new();
    let branch_entries = local.list(&index::dimension_prefix(BRANCH))?;
    for key in local.list(BUNDLES_PREFIX)? {
        if !key.ends_with(".loch") {
            continue;
        }
        let ref_ = index::ref_from_key(&key);
        let wanted = match scope {
            Scope::Bundle(only) => only == ref_,
            Scope::All => true,
            Scope::Project(project) => bundle_in_project(local, &key, project, &mut projects)?,
        };
        if !wanted {
            continue;
        }
        items.push(Item {
            key: key.clone(),
            kind: Kind::Bundle,
            owner: Owner::Bundle(ref_.clone()),
            data: None,
        });
        for entry in branch_entries
            .iter()
            .filter(|k| k.ends_with(&format!("/{}", ref_)))
        {
            items.push(Item {
                key: entry.clone(),
                kind: Kind::BranchEntry,
                owner: Owner::Bundle(ref_.clone()),
                data: None,
            });
        }
    }
    let project = match scope {
        Scope::Bundle(_) => return Ok(items),
        Scope::Project(project) => Some(project),
        Scope::All => None,
    };
    let transcripts = local.list(TRANSCRIPTS_PREFIX)?;
    for (agent, session, record) in local_sessions(local)? {
        if project.is_some_and(|p| record.project != p) {
            continue;
        }
        let owner = Owner::Session(session.clone());
        items.push(Item {
            key: attribution::edits_key(&agent, &session),
            kind: Kind::Record,
            owner: owner.clone(),
            data: None,
        });
        for (key, entry) in attribution::file_entries(&record) {
            items.push(Item {
                key,
                kind: Kind::FileEntry,
                owner: owner.clone(),
                data: Some(entry),
            });
        }
        for key in transcripts
            .iter()
            .filter(|k| transcript_of(k, &agent, &session))
        {
            items.push(Item {
                key: key.clone(),
                kind: Kind::Transcript,
                owner: owner.clone(),
                data: None,
            });
        }
    }
    Ok(items)
}

// -- what a push has sent ----------------------------------------------------------------

fn markers_path(uri: &str) -> std::path::PathBuf {
    snapshot::loch_home().join("pushed").join(format!(
        "{}.json",
        &snapshot::content_hash(uri.as_bytes())[..16]
    ))
}

fn load_markers(uri: &str) -> BTreeMap<String, String> {
    let mut markers = BTreeMap::new();
    let Some(Value::Object(body)) = std::fs::read_to_string(markers_path(uri))
        .ok()
        .and_then(|text| json::loads(&text).ok())
    else {
        return markers;
    };
    if let Some(Value::Object(keys)) = body.get("keys") {
        for (key, hash) in keys {
            if let Some(hash) = hash.as_str() {
                markers.insert(key.clone(), hash.to_string());
            }
        }
    }
    markers
}

/// A lost write only means sending some objects again.
fn save_markers(uri: &str, markers: &BTreeMap<String, String>) {
    let keys: Map = markers
        .iter()
        .map(|(k, v)| (k.clone(), Value::str(v.as_str())))
        .collect();
    let body = Value::Object(object! {"uri" => uri, "keys" => Value::Object(keys)});
    let _ = snapshot::write_atomically(
        &markers_path(uri),
        json::dumps(&body, json::COMPACT_UNICODE).as_bytes(),
    );
}

// -- push ---------------------------------------------------------------------------------

#[derive(Default)]
pub struct PushReport {
    pub dry_run: bool,
    /// Send everything in scope, whatever the markers say: the remote was
    /// emptied or replaced by something other than `delete --remote`, so the
    /// markers claim objects it no longer holds.
    pub force: bool,
    sent: BTreeMap<&'static str, usize>,
    pub up_to_date: usize,
    /// Left unsent because a tombstone covers it.
    pub tombstoned: usize,
    /// Sent, then deleted again because a delete raced the push.
    pub withdrawn: usize,
    /// Transcripts that aren't UTF-8 and so can't be redacted; never sent.
    pub unreadable: Vec<String>,
    /// Transcripts in which a secret survived redaction; never sent, and the
    /// push fails as `redaction-failed` once everything else has gone.
    pub redaction_failed: Vec<String>,
    pub redactions: IndexMap<String, usize>,
}

impl PushReport {
    pub fn new(dry_run: bool) -> Self {
        PushReport {
            dry_run,
            ..Default::default()
        }
    }

    pub fn total(&self) -> usize {
        self.sent.values().sum()
    }

    pub fn payload(&self) -> Map {
        let mut redactions = Map::new();
        for (rule, count) in &self.redactions {
            redactions.insert(rule.clone(), Value::int(*count as i64));
        }
        object! {
            "dryRun" => self.dry_run,
            "pushed" => counts_payload(&self.sent),
            "total" => self.total(),
            "upToDate" => self.up_to_date,
            "tombstoned" => self.tombstoned,
            "withdrawn" => self.withdrawn,
            "unreadable" => self.unreadable.iter().map(|k| Value::str(k.as_str())).collect::<Vec<_>>(),
            "redactionFailed" => self.redaction_failed.iter().map(|k| Value::str(k.as_str())).collect::<Vec<_>>(),
            "redactions" => Value::Object(redactions),
            "redacted" => self.redactions.values().sum::<usize>(),
        }
    }

    pub fn text(&self) -> String {
        let verb = if self.dry_run { "would push" } else { "pushed" };
        let parts: Vec<String> = KINDS
            .iter()
            .filter_map(|kind| {
                let n = *self.sent.get(kind.name()).unwrap_or(&0);
                (n > 0).then(|| format!("{} {}", n, kind.name()))
            })
            .collect();
        let mut text = if parts.is_empty() {
            "everything up to date\n".to_string()
        } else {
            format!("{} {}\n", verb, parts.join(", "))
        };
        if self.up_to_date > 0 && !parts.is_empty() {
            text.push_str(&format!("{} already up to date\n", self.up_to_date));
        }
        if self.tombstoned > 0 {
            text.push_str(&format!(
                "{} left unsent: deleted on the remote\n",
                self.tombstoned
            ));
        }
        if self.withdrawn > 0 {
            text.push_str(&format!(
                "{} withdrawn: deleted on the remote while pushing\n",
                self.withdrawn
            ));
        }
        if !self.unreadable.is_empty() {
            text.push_str(&format!(
                "{} transcript(s) not sent: not UTF-8, so they can't be redacted\n",
                self.unreadable.len()
            ));
        }
        let summary = redact::summarize(&self.redactions);
        if !summary.is_empty() {
            text.push_str(&format!("{}\n", summary));
        }
        text
    }
}

/// Sends what the local store holds that `remote` hasn't been sent, within
/// `scope`: bundles, and sessions' edit records, file index and transcripts. Bundles were redacted by `save`
/// and go as they are, so a ref is the same everywhere; transcripts are
/// redacted on the way out, since the hooks keep them verbatim.
pub fn push(
    local: &dyn Store,
    remote: &dyn Store,
    uri: &str,
    scope: Scope,
    report: &mut PushReport,
) -> Result<(), SyncError> {
    let dead = Dead::from_keys(
        &[
            remote.list(TOMBSTONES_PREFIX)?,
            local.list(TOMBSTONES_PREFIX)?,
        ]
        .concat(),
    );
    let items = plan(local, scope)?;
    let mut markers = load_markers(uri);
    let mut sent: Vec<(String, Owner)> = Vec::new();
    let result = send(local, remote, items, &dead, &mut markers, &mut sent, report);
    if !report.dry_run {
        save_markers(uri, &markers);
    }
    result?;
    if report.dry_run || sent.is_empty() {
        return redaction_verdict(report);
    }
    let after = Dead::from_keys(&remote.list(TOMBSTONES_PREFIX)?);
    for (key, owner) in &sent {
        if after.covers(owner) && !dead.covers(owner) {
            remote.delete(key)?;
            markers.remove(key);
            report.withdrawn += 1;
        }
    }
    if report.withdrawn > 0 {
        save_markers(uri, &markers);
    }
    redaction_verdict(report)
}

fn redaction_verdict(report: &PushReport) -> Result<(), SyncError> {
    if report.redaction_failed.is_empty() {
        return Ok(());
    }
    Err(SyncError::Redaction(format!(
        "{} transcript(s) not sent: a secret still matched after redaction",
        report.redaction_failed.len()
    )))
}

/// A record is sent merged with the remote's copy, and the merge is kept here
/// too, so a push never writes over turns another machine recorded for the
/// same session. Two pushes of one session at the same moment can still drop
/// the turns one of them added from the remote, until that machine's record
/// next changes and its push merges them back in.
fn merge_with_remote(
    local: &dyn Store,
    remote: &dyn Store,
    key: &str,
    data: Vec<u8>,
    hash: String,
) -> Result<(Vec<u8>, String), SyncError> {
    let Some(mine) = parse_record(&data) else {
        return Ok((data, hash));
    };
    let theirs = match remote.get(key) {
        Ok(bytes) => {
            parse_record(&bytes).filter(|r| r.agent == mine.agent && r.session == mine.session)
        }
        Err(StoreError::Missing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let Some(theirs) = theirs else {
        return Ok((data, hash));
    };
    let merged = mine.merge(&theirs);
    if merged.turns == mine.turns {
        return Ok((data, hash));
    }
    attribution::publish(local, &merged)?;
    for (entry, body) in attribution::file_entries(&merged) {
        remote.put(&entry, &body)?;
    }
    let body = local.get(key)?;
    let hash = snapshot::content_hash(&body);
    Ok((body, hash))
}

fn send(
    local: &dyn Store,
    remote: &dyn Store,
    items: Vec<Item>,
    dead: &Dead,
    markers: &mut BTreeMap<String, String>,
    sent: &mut Vec<(String, Owner)>,
    report: &mut PushReport,
) -> Result<(), SyncError> {
    for item in items {
        if dead.covers(&item.owner) {
            report.tombstoned += 1;
            continue;
        }
        let data = match item.data {
            Some(data) => data,
            None => match local.get(&item.key) {
                Ok(data) => data,
                Err(StoreError::Missing(_)) => continue,
                Err(e) => return Err(e.into()),
            },
        };
        let hash = snapshot::content_hash(&data);
        if !report.force && markers.get(&item.key) == Some(&hash) {
            report.up_to_date += 1;
            continue;
        }
        let (data, hash) = if item.kind == Kind::Record && !report.dry_run {
            merge_with_remote(local, remote, &item.key, data, hash)?
        } else {
            (data, hash)
        };
        let body = if item.kind == Kind::Transcript {
            let Ok(text) = std::str::from_utf8(&data) else {
                report.unreadable.push(item.key);
                continue;
            };
            // Redaction is deterministic, so this transcript would fail every
            // push; it is skipped and named, and the rest still goes.
            let Ok(scrubbed) = redact::redact(text) else {
                report.redaction_failed.push(item.key);
                continue;
            };
            for (rule, count) in scrubbed.counts {
                *report.redactions.entry(rule).or_insert(0) += count;
            }
            scrubbed.text.into_bytes()
        } else {
            data
        };
        if !report.dry_run {
            remote.put(&item.key, &body)?;
            markers.insert(item.key.clone(), hash);
        }
        *report.sent.entry(item.kind.name()).or_insert(0) += 1;
        sent.push((item.key, item.owner));
    }
    Ok(())
}

// -- delete -------------------------------------------------------------------------------

/// Removes a session's record, file index and transcripts from `store`, and
/// returns the keys removed. The file index is found from the record, so a
/// store holding the entries without the record keeps them until `reindex`.
pub fn delete_session(
    store: &dyn Store,
    agent: &str,
    session: &str,
) -> Result<Vec<String>, StoreError> {
    let mut removed = Vec::new();
    let key = attribution::edits_key(agent, session);
    if let Some(record) = store.get(&key).ok().and_then(|d| parse_record(&d)) {
        for (entry, _) in attribution::file_entries(&record) {
            store.delete(&entry)?;
            removed.push(entry);
        }
        store.delete(&key)?;
        removed.push(key);
    }
    for transcript in store.list(&trace::transcripts_prefix(agent))? {
        if transcript_of(&transcript, agent, session) {
            store.delete(&transcript)?;
            removed.push(transcript);
        }
    }
    Ok(removed)
}

/// The agents whose records or transcripts in `store` name `session`.
pub fn agents_of_session(store: &dyn Store, session: &str) -> Result<Vec<String>, StoreError> {
    let mut agents = Vec::new();
    for agent in agent::all() {
        let id = agent.id();
        let has_record = store.get(&attribution::edits_key(id, session)).is_ok();
        let has_transcript = store
            .list(&trace::transcripts_prefix(id))?
            .iter()
            .any(|k| transcript_of(k, id, session));
        if has_record || has_transcript {
            agents.push(id.to_string());
        }
    }
    Ok(agents)
}

// -- pull ---------------------------------------------------------------------------------

#[derive(Default)]
pub struct PullReport {
    pub records: usize,
    pub transcripts: usize,
    pub up_to_date: usize,
    pub tombstones: usize,
    /// Sessions and bundles removed from this machine because a remote
    /// tombstone covers them.
    pub forgotten: usize,
    /// Keys the remote named that can't be a place in the local store, or
    /// records that don't say what their key does. Never written.
    pub refused: usize,
}

impl PullReport {
    pub fn payload(&self) -> Map {
        object! {
            "records" => self.records,
            "transcripts" => self.transcripts,
            "upToDate" => self.up_to_date,
            "tombstones" => self.tombstones,
            "forgotten" => self.forgotten,
            "refused" => self.refused,
        }
    }

    pub fn text(&self) -> String {
        let mut text = if self.records + self.transcripts == 0 {
            "already up to date\n".to_string()
        } else {
            format!(
                "pulled {} record(s), {} transcript(s)\n",
                self.records, self.transcripts
            )
        };
        if self.forgotten > 0 {
            text.push_str(&format!(
                "forgot {} deleted on the remote\n",
                self.forgotten
            ));
        }
        if self.refused > 0 {
            text.push_str(&format!(
                "refused {} object(s) whose key or content was malformed\n",
                self.refused
            ));
        }
        text
    }
}

/// Fetches the edit records `remote` holds for `project` (every one, for
/// `None`) that know more turns than this machine's copy, with their
/// transcripts, and forgets what the remote's tombstones cover. Bundles are
/// not fetched; `restore` fetches the one it is asked for.
///
/// Everything the remote names is someone else's input: a key is written only
/// if it is a place inside the store, a session only if its id can be a file
/// name, and a record only if it says what its key does.
pub fn pull(
    local: &FileStore,
    remote: &dyn Store,
    project: Option<&str>,
    report: &mut PullReport,
) -> Result<(), SyncError> {
    for key in remote.list(TOMBSTONES_PREFIX)? {
        if !store::is_safe_key(&key) {
            report.refused += 1;
            continue;
        }
        if local.exists(&key) {
            continue;
        }
        match remote.get(&key) {
            Ok(data) => local.put(&key, &data)?,
            Err(StoreError::Missing(_)) => continue,
            Err(e) => return Err(e.into()),
        }
        report.tombstones += 1;
    }
    let dead = Dead::from_keys(&local.list(TOMBSTONES_PREFIX)?);
    forget(local, &dead, report)?;

    let scoped: Option<BTreeSet<String>> = match project {
        Some(project) => Some(
            remote
                .list(&index::dimension_prefix(FILE))?
                .iter()
                .filter_map(|key| file_index_session(key, project))
                .collect(),
        ),
        None => None,
    };
    let transcripts = remote.list(TRANSCRIPTS_PREFIX)?;
    for key in remote.list(EDITS_PREFIX)? {
        let Some((agent, session)) = parse_edits_key(&key) else {
            report.refused += 1;
            continue;
        };
        if dead.covers(&Owner::Session(session.clone()))
            || scoped.as_ref().is_some_and(|s| !s.contains(&session))
        {
            continue;
        }
        let data = match remote.get(&key) {
            Ok(data) => data,
            Err(StoreError::Missing(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let Some(record) = parse_record(&data)
            .filter(|r| r.agent == agent && r.session == session)
            .filter(|r| project.is_none_or(|p| r.project == p))
        else {
            report.refused += 1;
            continue;
        };
        // Merged, never replaced: this machine may have recorded turns of the
        // same session (a restore that kept its id) that the remote lacks.
        let have = attribution::load_stored(local, &agent, &session);
        let merged = match &have {
            Some(have) => have.merge(&record),
            None => record,
        };
        let newer = have.is_none_or(|have| merged.turns != have.turns);
        if newer {
            attribution::publish(local, &merged)?;
            report.records += 1;
        } else {
            report.up_to_date += 1;
        }
        for transcript in transcripts
            .iter()
            .filter(|k| transcript_of(k, &agent, &session))
        {
            if !store::is_safe_key(transcript) {
                report.refused += 1;
                continue;
            }
            if !newer && local.exists(transcript) {
                continue;
            }
            match remote.get(transcript) {
                Ok(data) => local.put(transcript, &data)?,
                Err(StoreError::Missing(_)) => continue,
                Err(e) => return Err(e.into()),
            }
            report.transcripts += 1;
        }
    }
    Ok(())
}

/// Removes from this machine what tombstones cover: loch's own copies, never
/// an agent's transcript in its own directory.
fn forget(local: &FileStore, dead: &Dead, report: &mut PullReport) -> Result<(), SyncError> {
    for ref_ in &dead.refs {
        let key = index::bundle_key(ref_);
        if !local.exists(&key) {
            continue;
        }
        if index::delete_bundle(local, ref_).is_err() {
            local.delete(&key)?;
        }
        report.forgotten += 1;
    }
    if dead.sessions.is_empty() {
        return Ok(());
    }
    let transcripts = local.list(TRANSCRIPTS_PREFIX)?;
    for (agent, session) in &dead.sessions {
        if !agent::is_safe_session_id(session) || agent::by_id(agent).is_none() {
            continue;
        }
        let mut forgot = attribution::forget_local(local, agent, session)?;
        for key in transcripts
            .iter()
            .filter(|k| transcript_of(k, agent, session))
        {
            local.delete(key)?;
            forgot = true;
        }
        if forgot {
            report.forgotten += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_keys_are_matched_to_their_session() {
        let id = "11111111-2222-3333-4444-555555555555";
        assert!(transcript_of(
            &format!("transcripts/claude-code/-repo/{}.jsonl", id),
            "claude-code",
            id
        ));
        assert!(transcript_of(
            &format!("transcripts/codex/rollout-2026-01-01T00-00-00-{}.jsonl", id),
            "codex",
            id
        ));
        assert!(!transcript_of(
            &format!("transcripts/claude-code/-repo/x{}.jsonl", id),
            "claude-code",
            id
        ));
        assert!(!transcript_of(
            &format!("transcripts/claude-code/.hidden/{}.jsonl", id),
            "claude-code",
            id
        ));
        assert!(!transcript_of(
            &format!("transcripts/codex/{}.jsonl", id),
            "claude-code",
            id
        ));
    }

    #[test]
    fn tombstones_cover_bundles_and_sessions() {
        let dead = Dead::from_keys(&[
            bundle_tombstone("abc"),
            session_tombstone("codex", "s1"),
            "tombstones/other/x".into(),
        ]);
        assert!(dead.covers(&Owner::Bundle("abc".into())));
        assert!(dead.covers(&Owner::Session("s1".into())));
        assert!(!dead.covers(&Owner::Session("abc".into())));
    }

    #[test]
    fn file_index_keys_name_their_project() {
        let key = index::index_key(FILE, Some("git:abc:src/a.rs"), "s1");
        assert_eq!(file_index_session(&key, "git:abc"), Some("s1".into()));
        assert_eq!(file_index_session(&key, "git:ab"), None);
    }
}
