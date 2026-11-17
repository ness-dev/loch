//! The store interface, as in `loch/store.py`. Only what the ported commands
//! use is here; the rest arrives with the commands that need it.

use crate::claude::home_dir;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The message is for the commands that report it; the hook only needs to
/// know that the put failed.
#[derive(Debug)]
#[allow(dead_code)]
pub enum StoreError {
    /// The store didn't answer (`store-unreachable`).
    Unreachable(String),
    Invalid(String),
}

pub trait Store {
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError>;
}

pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FileStore { root: root.into() }
    }
}

impl Store for FileStore {
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::Unreachable(e.to_string()))?;
        }
        fs::write(&path, data).map_err(|e| StoreError::Unreachable(e.to_string()))
    }
}

pub fn default_store() -> String {
    Path::new(&home_dir())
        .join(".loch")
        .join("store")
        .to_string_lossy()
        .into_owned()
}

pub fn resolve_store_uri(explicit: Option<&str>) -> String {
    if let Some(uri) = explicit {
        return uri.to_string();
    }
    std::env::var("LOCH_STORE").unwrap_or_else(|_| default_store())
}

pub fn create_store(uri: &str) -> Result<Box<dyn Store>, StoreError> {
    if let Some(rest) = uri.strip_prefix("s3://") {
        let bucket = rest.split('/').next().unwrap_or("");
        if bucket.is_empty() {
            return Err(StoreError::Invalid(format!(
                "invalid S3 store URI: {}",
                uri
            )));
        }
        return Err(StoreError::Unreachable(
            "the S3 backend is not ported to the Rust build yet".into(),
        ));
    }
    if let Some(rest) = uri.strip_prefix("file://") {
        return Ok(Box::new(FileStore::new(file_uri_path(rest))));
    }
    Ok(Box::new(FileStore::new(abspath(uri))))
}

/// `unquote(urlparse(uri).path)`: the netloc and any query or fragment are
/// dropped, and percent-escapes decoded as UTF-8 with replacement.
fn file_uri_path(rest: &str) -> String {
    let path = match rest.find('/') {
        Some(slash) => &rest[slash..],
        None => "",
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                decoded.push(value);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `os.path.abspath`: joined onto the working directory and normalised
/// lexically, without touching the filesystem.
pub fn abspath(path: &str) -> String {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut parts: Vec<Component> = Vec::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    let normal: PathBuf = parts.iter().collect();
    normal.to_string_lossy().into_owned()
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

    #[test]
    fn abspath_normalises_lexically() {
        assert_eq!(abspath("/a/./b/../c"), "/a/c");
        assert_eq!(abspath("/.."), "/");
    }
}
