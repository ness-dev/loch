//! Snapshots of a workspace's files, taken on demand the way git's index is:
//! nothing runs between snapshots.
//!
//! A snapshot is the sorted list of every file loch can see in the workspace
//! (what `.gitignore` and friends don't exclude) with each one's content hash.
//! Its id is the hash of that list, so an unchanged workspace snapshots to the
//! same id. Contents live once each in `~/.loch/objects`, addressed by hash and
//! shared across workspaces, which is what makes a second worktree of the same
//! repository nearly free.
//!
//! **The stat cache is the whole performance story.** A file whose size and
//! mtime match the last snapshot's is not read again. Git's one trap applies:
//! a file rewritten within the same mtime tick as the snapshot that recorded it
//! looks unchanged. So an entry is only trusted when its mtime is comfortably
//! older than the snapshot that cached it; anything newer is rehashed.

use crate::claude::home_dir;
use crate::sys;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A file modified this close to the snapshot that cached it is rehashed next
/// time regardless of its stat: the racily-clean window.
const RACY_NANOS: i128 = 2_000_000_000;

#[derive(Debug)]
pub struct SnapshotError(pub String);

fn io(error: std::io::Error, path: &Path) -> SnapshotError {
    SnapshotError(sys::os_error(&error, &path.to_string_lossy()))
}

pub fn loch_home() -> PathBuf {
    PathBuf::from(home_dir()).join(".loch")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn content_hash(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// The directory a session's files are relative to: the nearest ancestor of
/// `cwd` holding a `.git` entry — a marker, read like any other file; nothing
/// asks git — or `cwd` itself where there is none. Symlinks are resolved, as
/// Claude resolves them, so one checkout has one root however it was entered.
pub fn workspace_root(cwd: &str) -> String {
    let start = sys::realpath_strict(cwd).unwrap_or_else(|| sys::abspath(cwd));
    let mut dir = Path::new(&start);
    loop {
        if dir.join(".git").exists() {
            return dir.to_string_lossy().into_owned();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return start,
        }
    }
}

/// Whether `root` has a `.git` marker, so that a project id can come from it.
pub fn has_git_marker(root: &str) -> bool {
    Path::new(root).join(".git").exists()
}

fn object_path(hash: &str) -> PathBuf {
    loch_home()
        .join("objects")
        .join(&hash[..2])
        .join(&hash[2..])
}

fn write_object(hash: &str, data: &[u8]) -> Result<(), SnapshotError> {
    let path = object_path(hash);
    if path.exists() {
        return Ok(());
    }
    let parent = path.parent().expect("objects have a directory");
    fs::create_dir_all(parent).map_err(|e| io(e, parent))?;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("writing to a Vec");
    let compressed = encoder.finish().expect("finishing into a Vec");
    // Written aside and renamed, so a reader never sees half an object.
    let staged = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&staged, compressed).map_err(|e| io(e, &staged))?;
    fs::rename(&staged, &path).map_err(|e| io(e, &path))
}

pub fn read_object(hash: &str) -> Result<Vec<u8>, SnapshotError> {
    let path = object_path(hash);
    let compressed = fs::read(&path).map_err(|e| io(e, &path))?;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(&compressed[..])
        .read_to_end(&mut out)
        .map_err(|e| io(e, &path))?;
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

fn manifest_bytes(entries: &[Entry]) -> Vec<u8> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!("{}\t{}\t{}\n", entry.hash, entry.size, entry.path));
    }
    out.into_bytes()
}

/// A snapshot's files, sorted by path.
pub fn read_snapshot(id: &str) -> Result<Vec<Entry>, SnapshotError> {
    let text = String::from_utf8(read_object(id)?)
        .map_err(|_| SnapshotError(format!("snapshot {} is not UTF-8", id)))?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(hash), Some(size), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(SnapshotError(format!("snapshot {} is malformed", id)));
        };
        entries.push(Entry {
            path: path.into(),
            hash: hash.into(),
            size: size.parse().unwrap_or(0),
        });
    }
    Ok(entries)
}

/// Per-workspace state: the stat cache and the id of its latest snapshot.
fn state_dir(root: &str) -> PathBuf {
    loch_home()
        .join("workspaces")
        .join(&content_hash(root.as_bytes())[..16])
}

struct Cached {
    size: u64,
    mtime: i128,
    hash: String,
}

fn load_cache(root: &str) -> (HashMap<String, Cached>, i128) {
    let Ok(text) = fs::read_to_string(state_dir(root).join("index")) else {
        return (HashMap::new(), 0);
    };
    let mut lines = text.lines();
    let taken: i128 = lines.next().and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut cache = HashMap::new();
    for line in lines {
        let mut parts = line.splitn(4, '\t');
        if let (Some(size), Some(mtime), Some(hash), Some(path)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        {
            if let (Ok(size), Ok(mtime)) = (size.parse(), mtime.parse()) {
                cache.insert(
                    path.to_string(),
                    Cached {
                        size,
                        mtime,
                        hash: hash.into(),
                    },
                );
            }
        }
    }
    (cache, taken)
}

fn save_cache(root: &str, taken: i128, cache: &[(String, Cached)]) -> Result<(), SnapshotError> {
    let dir = state_dir(root);
    fs::create_dir_all(&dir).map_err(|e| io(e, &dir))?;
    let mut text = format!("{}\n", taken);
    for (path, cached) in cache {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            cached.size, cached.mtime, cached.hash, path
        ));
    }
    let path = dir.join("index");
    let staged = dir.join(format!("index.tmp{}", std::process::id()));
    fs::write(&staged, text).map_err(|e| io(e, &staged))?;
    fs::rename(&staged, &path).map_err(|e| io(e, &path))
}

/// The latest snapshot taken of `root`, by any session.
pub fn latest(root: &str) -> Option<String> {
    fs::read_to_string(state_dir(root).join("latest"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn mtime_nanos(meta: &fs::Metadata) -> i128 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
    }
    #[cfg(not(unix))]
    {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0)
    }
}

fn now_nanos() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Every regular file under `root` that the user's ignore rules let through,
/// as relative paths with `/` separators. `.git` itself is never a file of
/// the workspace; dotfiles otherwise are (`.github`, `.env.example`).
/// Stops after `limit` files, so an enormous directory costs a bounded walk.
pub fn visible_files(root: &str, limit: usize) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Honour .gitignore whether or not this is a git repository.
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        files.push((relative, entry.path().to_path_buf()));
        if files.len() >= limit {
            break;
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// More files than this and it isn't a project: a session started in a
/// directory full of unrelated things. Snapshotting it would cost minutes on
/// every turn, so attribution is declined instead.
pub const MAX_FILES: usize = 100_000;

/// Whether a root is one loch will snapshot at all. Sessions run from `/` and
/// from the home directory are real — one machine had 486 in `/` — and walking
/// either is a walk of everything.
pub fn snapshottable(root: &str) -> bool {
    let home = sys::realpath_strict(&home_dir()).unwrap_or_else(home_dir);
    root != "/" && root != home && !root.is_empty()
}

/// Snapshots `root` and returns the snapshot's id.
pub fn snapshot(root: &str) -> Result<String, SnapshotError> {
    if !snapshottable(root) {
        return Err(SnapshotError(format!(
            "{} is not a project to snapshot",
            root
        )));
    }
    let files = visible_files(root, MAX_FILES + 1);
    if files.len() > MAX_FILES {
        return Err(SnapshotError(format!(
            "{} holds more than {} files",
            root, MAX_FILES
        )));
    }
    let (cache, cache_taken) = load_cache(root);
    let taken = now_nanos();
    let mut entries = Vec::new();
    let mut fresh: Vec<(String, Cached)> = Vec::new();
    for (relative, path) in files {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let size = meta.len();
        let mtime = mtime_nanos(&meta);
        let trusted = cache
            .get(&relative)
            .filter(|c| c.size == size && c.mtime == mtime && mtime + RACY_NANOS < cache_taken);
        let hash = match trusted {
            Some(cached) => cached.hash.clone(),
            None => {
                let Ok(data) = fs::read(&path) else {
                    continue;
                };
                let hash = content_hash(&data);
                write_object(&hash, &data)?;
                hash
            }
        };
        entries.push(Entry {
            path: relative.clone(),
            hash: hash.clone(),
            size,
        });
        fresh.push((relative, Cached { size, mtime, hash }));
    }
    let manifest = manifest_bytes(&entries);
    let id = content_hash(&manifest);
    write_object(&id, &manifest)?;
    save_cache(root, taken, &fresh)?;
    let dir = state_dir(root);
    fs::write(dir.join("latest"), &id).map_err(|e| io(e, &dir))?;
    Ok(id)
}
