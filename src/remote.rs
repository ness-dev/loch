//! Named remotes, git style: `~/.loch/config` maps names to store URIs, and
//! `push`, `pull` and every command that reaches past this machine name one,
//! or take the default.
//!
//! The local store (`store::local_store`) is not a remote and has no name;
//! everything is recorded there first.

use crate::json::{self, Map, Value};
use crate::object;
use crate::snapshot;
use crate::store::{self, Store};

pub const DEFAULT_NAME: &str = "origin";

/// The store a command reaches past this machine to.
pub struct Remote {
    /// `None` for one given as a URI, or by `LOCH_REMOTE`/`LOCH_STORE`.
    pub name: Option<String>,
    pub uri: String,
    /// Named by `LOCH_STORE`, which meant the store the hooks wrote to before
    /// there were remotes. It still works, and says so.
    pub deprecated: bool,
}

impl Remote {
    pub fn open(&self) -> Result<Box<dyn Store>, RemoteError> {
        store::create_store(&self.uri).map_err(|e| RemoteError::new("invalid-uri", e.0))
    }

    /// How the remote is named in prose: its name, else its URI.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.uri.clone())
    }

    pub fn payload(&self) -> Value {
        Value::Object(object! {
            "name" => Value::opt_str(self.name.as_deref()),
            "uri" => self.uri.as_str(),
        })
    }

    pub fn warning(&self) -> Option<String> {
        self.deprecated.then(|| {
            "LOCH_STORE now names a remote; set LOCH_REMOTE, or `loch remote add origin <uri>`\n"
                .to_string()
        })
    }
}

#[derive(Debug)]
pub struct RemoteError {
    pub code: &'static str,
    pub message: String,
}

impl RemoteError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        RemoteError {
            code,
            message: message.into(),
        }
    }
}

pub fn config_path() -> std::path::PathBuf {
    snapshot::loch_home().join("config")
}

/// The remotes, in the order they were added. A config that doesn't exist
/// holds none; one that exists and won't parse is an error, since treating it
/// as empty would let `remote add` overwrite it.
pub fn remotes() -> Result<Map, RemoteError> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => {
            return Err(RemoteError::new(
                "config-unreadable",
                format!("{}: {}", path.display(), e),
            ))
        }
    };
    let unreadable = || {
        RemoteError::new(
            "config-unreadable",
            format!("{} is not a loch config", path.display()),
        )
    };
    let config = json::loads(&text).map_err(|_| unreadable())?;
    let Value::Object(config) = config else {
        return Err(unreadable());
    };
    match config.get("remotes") {
        None => Ok(Map::new()),
        Some(Value::Object(remotes)) if remotes.values().all(|v| v.as_str().is_some()) => {
            Ok(remotes.clone())
        }
        Some(_) => Err(unreadable()),
    }
}

/// Written aside and renamed, like every file loch keeps, so a killed write
/// leaves the old config or the new one.
fn write_remotes(remotes: Map) -> Result<(), RemoteError> {
    let config = Value::Object(object! {"remotes" => Value::Object(remotes)});
    snapshot::write_atomically(
        &config_path(),
        json::dumps(&config, json::DEFAULT).as_bytes(),
    )
    .map_err(|e| {
        RemoteError::new(
            "config-unreadable",
            format!("{}: {}", config_path().display(), e),
        )
    })
}

/// A bundle ref: 64 lowercase hex digits. No remote may be named like one,
/// so `delete --remote <ref>` can be read the way it was meant.
pub fn looks_like_ref(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !looks_like_ref(name)
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A name or a URI where a remote is asked for, as `git push <url>` takes
/// either. Every URI holds a `/`, and no name can.
fn is_uri(text: &str) -> bool {
    text.contains('/')
}

/// The URI as the config keeps it. A scheme the store doesn't know is
/// refused: `create_store` takes anything else as a relative path, so
/// `ssh://host` would become a local directory named `ssh:` and a push to it
/// would succeed. A relative path is made absolute, so a remote doesn't
/// depend on where it was added from.
fn normalize(uri: &str) -> Result<String, RemoteError> {
    if let Some((scheme, _)) = uri.split_once("://") {
        if !matches!(scheme, "s3" | "file") {
            return Err(RemoteError::new(
                "invalid-uri",
                format!("unsupported store URI {} (a path, file:// or s3://)", uri),
            ));
        }
    } else if !uri.starts_with('/') {
        return Ok(crate::sys::abspath(uri));
    }
    store::create_store(uri).map_err(|e| RemoteError::new("invalid-uri", e.0))?;
    Ok(uri.into())
}

pub fn add(name: &str, uri: &str) -> Result<Remote, RemoteError> {
    if !valid_name(name) {
        return Err(RemoteError::new(
            "usage",
            format!(
                "{:?} is not a remote name (letters, digits, '.', '_' and '-')",
                name
            ),
        ));
    }
    let uri = normalize(uri)?;
    let mut remotes = remotes()?;
    if remotes.contains_key(name) {
        return Err(RemoteError::new(
            "remote-exists",
            format!("remote {} already exists", name),
        ));
    }
    remotes.insert(name.into(), Value::str(uri.as_str()));
    write_remotes(remotes)?;
    Ok(Remote {
        name: Some(name.into()),
        uri,
        deprecated: false,
    })
}

pub fn remove(name: &str) -> Result<Remote, RemoteError> {
    let mut remotes = remotes()?;
    let Some(uri) = remotes.shift_remove(name) else {
        return Err(unknown(name));
    };
    write_remotes(remotes)?;
    Ok(Remote {
        name: Some(name.into()),
        uri: uri.as_str().unwrap_or_default().into(),
        deprecated: false,
    })
}

pub fn list() -> Result<Vec<Remote>, RemoteError> {
    Ok(remotes()?
        .into_iter()
        .map(|(name, uri)| Remote {
            name: Some(name),
            uri: uri.as_str().unwrap_or_default().into(),
            deprecated: false,
        })
        .collect())
}

fn unknown(name: &str) -> RemoteError {
    RemoteError::new("unknown-remote", format!("no remote named {}", name))
}

fn from_env(variable: &str) -> Option<String> {
    std::env::var(variable).ok().filter(|v| !v.is_empty())
}

/// The remote a command means: the one it names (or the URI it gives), else
/// `LOCH_REMOTE`, else the deprecated `LOCH_STORE`, else `origin`, else the
/// only remote there is. Several remotes and no `origin` is ambiguous rather
/// than a guess, since a push to the wrong one publishes transcripts.
pub fn resolve(asked: Option<&str>) -> Result<Remote, RemoteError> {
    if let Some(asked) = asked.filter(|a| !a.is_empty()) {
        if is_uri(asked) {
            return Ok(Remote {
                name: None,
                uri: normalize(asked)?,
                deprecated: false,
            });
        }
        let remotes = remotes()?;
        return match remotes.get(asked) {
            Some(uri) => Ok(Remote {
                name: Some(asked.into()),
                uri: uri.as_str().unwrap_or_default().into(),
                deprecated: false,
            }),
            None => Err(unknown(asked)),
        };
    }
    for (variable, deprecated) in [("LOCH_REMOTE", false), ("LOCH_STORE", true)] {
        if let Some(uri) = from_env(variable) {
            return Ok(Remote {
                name: None,
                uri: normalize(&uri)?,
                deprecated,
            });
        }
    }
    let remotes = remotes()?;
    match default_name(&remotes) {
        Some(name) => Ok(Remote {
            uri: remotes[name.as_str()].as_str().unwrap_or_default().into(),
            name: Some(name),
            deprecated: false,
        }),
        None if remotes.is_empty() => Err(RemoteError::new(
            "no-remote",
            "no remote configured; `loch remote add origin <uri>`, or set LOCH_REMOTE",
        )),
        None => Err(RemoteError::new(
            "ambiguous-remote",
            format!(
                "{} remotes and none named {}; name one",
                remotes.len(),
                DEFAULT_NAME
            ),
        )),
    }
}

/// The configured remote a command takes when it names none: `origin`, or
/// the only one.
pub fn default_name(remotes: &Map) -> Option<String> {
    if remotes.contains_key(DEFAULT_NAME) {
        Some(DEFAULT_NAME.into())
    } else if remotes.len() == 1 {
        remotes.keys().next().cloned()
    } else {
        None
    }
}
