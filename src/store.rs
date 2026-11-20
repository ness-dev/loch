//! The store interface: a local directory or any
//! S3-compatible endpoint behind the same five operations.

use crate::claude::home_dir;
use crate::sys;
use std::fs;
use std::path::Path;

/// The failures a caller can act on, kept apart for the reasons
/// `cli.store_errors` gives. There is no "missing dependency": the Rust build
/// carries its S3 client, so `s3-extra-missing` can't arise.
#[derive(Debug)]
pub enum StoreError {
    /// The store answered and the object isn't there (`bundle-not-found`).
    Missing(String),
    /// The store didn't answer, or refused (`store-unreachable`).
    Unreachable(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            StoreError::Missing(m) | StoreError::Unreachable(m) => f.write_str(m),
        }
    }
}

pub trait Store {
    fn describe(&self) -> String;
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError>;
    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError>;
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
    /// Deleting an absent key succeeds: index entries are derived, so a repair
    /// pass has to be able to remove ones a half-finished save never wrote.
    fn delete(&self, key: &str) -> Result<(), StoreError>;
}

pub struct FileStore {
    root: String,
}

impl FileStore {
    pub fn new(root: impl Into<String>) -> Self {
        FileStore { root: root.into() }
    }

    /// `Path(root) / key`, which collapses repeated and trailing slashes.
    fn path(&self, key: &str) -> String {
        let mut path = self.root.trim_end_matches('/').to_string();
        if path.is_empty() && self.root.starts_with('/') {
            path.push('/');
        }
        for part in key.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(part);
        }
        path
    }
}

fn unreachable(error: &std::io::Error, path: &str) -> StoreError {
    StoreError::Unreachable(sys::os_error(error, path))
}

impl Store for FileStore {
    fn describe(&self) -> String {
        self.root.clone()
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let path = self.path(key);
        let parent = sys::dirname(&path);
        fs::create_dir_all(&parent).map_err(|e| unreachable(&e, &parent))?;
        fs::write(&path, data).map_err(|e| unreachable(&e, &path))
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.path(key);
        fs::read(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => StoreError::Missing(format!("no object at {}", key)),
            _ => unreachable(&error, &path),
        })
    }

    /// String prefix rather than directory descent, so both backends answer
    /// the same question for a prefix that stops mid-segment.
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let root = self.path("");
        if !Path::new(&root).is_dir() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        walk(Path::new(&root), "", &mut keys);
        keys.retain(|key| key.starts_with(prefix));
        keys.sort_by(|a, b| crate::py::cmp(a, b));
        Ok(keys)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let path = self.path(key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(unreachable(&error, &path)),
        }
    }
}

/// `root.rglob("*")` keeping files: directories are descended without
/// following symlinks, and a symlink to a file counts as a file.
fn walk(dir: &Path, relative: &str, keys: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let key = if relative.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", relative, name)
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&entry.path(), &key, keys);
        } else if entry.path().is_file() {
            keys.push(key);
        }
    }
}

pub fn default_store() -> String {
    sys::join(&sys::join(&home_dir(), ".loch"), "store")
}

/// This machine's own store, which the hooks, `backfill`, `blame`, `log` and
/// `ask` use and nothing else. They never reach the network: a remote that
/// doesn't answer would hold a turn for its timeout, and Codex kills a
/// `SessionEnd` hook at 3s.
pub fn local_store() -> FileStore {
    FileStore::new(default_store())
}

pub fn resolve_store_uri(explicit: Option<&str>) -> String {
    if let Some(uri) = explicit {
        return uri.to_string();
    }
    std::env::var("LOCH_STORE").unwrap_or_else(|_| default_store())
}

#[derive(Debug)]
pub struct InvalidUri(pub String);

pub fn create_store(uri: &str) -> Result<Box<dyn Store>, InvalidUri> {
    if let Some(rest) = uri.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return Err(InvalidUri(format!("invalid S3 store URI: {}", uri)));
        }
        // `re.sub(r"/$", "", prefix)`: one trailing slash, not all of them.
        let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
        return Ok(Box::new(crate::s3::S3Store::new(bucket, prefix)));
    }
    if let Some(rest) = uri.strip_prefix("file://") {
        return Ok(Box::new(FileStore::new(file_uri_path(rest))));
    }
    if uri.starts_with('/') {
        return Ok(Box::new(FileStore::new(uri)));
    }
    Ok(Box::new(FileStore::new(sys::abspath(uri))))
}

/// `unquote(urlparse(uri).path)`.
fn file_uri_path(rest: &str) -> String {
    let path = match rest.find('/') {
        Some(slash) => &rest[slash..],
        None => "",
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    crate::index::unquote(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode_like_urllib() {
        assert_eq!(file_uri_path("/tmp/a%20b"), "/tmp/a b");
        assert_eq!(file_uri_path("host/tmp/x?q=1#f"), "/tmp/x");
        assert_eq!(file_uri_path("/tmp/%zz%2"), "/tmp/%zz%2");
    }
}
