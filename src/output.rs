//! The result envelope and its two renderings.

use crate::json::{self, Map, Value};
use std::io::Write;

pub const SCHEMA: i64 = 3;

pub const JSON_FLAG: &str = "--json";

/// What a command produced, in both renderings. Building the payload and the
/// prose in one place is what keeps them from drifting apart.
pub struct Outcome {
    pub command: String,
    pub payload: Map,
    pub text: String,
    pub warnings: Vec<String>,
}

impl Outcome {
    pub fn new(command: &str) -> Self {
        Outcome {
            command: command.to_string(),
            payload: Map::new(),
            text: String::new(),
            warnings: Vec::new(),
        }
    }

    pub fn with(command: &str, payload: Map, text: String) -> Self {
        Outcome {
            command: command.into(),
            payload,
            text,
            warnings: Vec::new(),
        }
    }
}

/// A failure the caller is meant to act on. `code` is a stable token —
/// consumers branch on it, so reword `message` freely but not `code`.
#[derive(Debug)]
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
    pub payload: Map,
    pub warnings: Vec<String>,
    pub exit_status: i32,
    pub prose: Option<String>,
}

impl CommandError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        CommandError {
            code,
            message: message.into(),
            payload: Map::new(),
            warnings: Vec::new(),
            exit_status: 1,
            prose: None,
        }
    }
}

pub type Result<T> = std::result::Result<T, CommandError>;

pub fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T> {
    Err(CommandError::new(code, message))
}

/// The envelope's own keys, which no payload may use.
pub const RESERVED: &[&str] = &["schema", "ok", "command", "code", "error"];

pub fn document(command: &str, ok: bool, payload: Map, failure: Option<(&str, &str)>) -> Value {
    let mut body = Map::new();
    body.insert("schema".into(), Value::int(SCHEMA));
    body.insert("ok".into(), Value::Bool(ok));
    body.insert("command".into(), Value::str(command));
    if let Some((code, error)) = failure {
        body.insert("code".into(), Value::str(code));
        body.insert("error".into(), Value::str(error));
    }
    for (key, value) in payload {
        // A payload key equal to an envelope key would overwrite it, as
        // `install`'s `command` once did, turning "install" into a path.
        debug_assert!(
            !RESERVED.contains(&key.as_str()),
            "payload key {} is the envelope's",
            key
        );
        if RESERVED.contains(&key.as_str()) {
            continue;
        }
        body.insert(key, value);
    }
    Value::Object(body)
}

/// Text that Python would write to a text-mode stream. A surrogate is
/// unencodable there, so Python would raise; writing it with escaping
/// replaced keeps the stream valid instead.
fn write_text(stream: &mut dyn Write, text: &str) {
    let bytes = crate::py::encode_strict(text)
        .unwrap_or_else(|_| crate::py::decode_replace(text.as_bytes()).into_bytes());
    let _ = stream.write_all(&bytes);
}

pub fn emit(outcome: Outcome, as_json: bool) {
    if as_json {
        write_document(&document(&outcome.command, true, outcome.payload, None));
        return;
    }
    // Warnings are stderr-only prose; JSON carries the same facts as per-item
    // status, so nothing but the document is written in that mode.
    let mut stderr = std::io::stderr();
    for warning in &outcome.warnings {
        write_text(&mut stderr, warning);
    }
    let mut stdout = std::io::stdout();
    write_text(&mut stdout, &outcome.text);
    let _ = stdout.flush();
}

pub fn emit_error(command: &str, error: CommandError, as_json: bool) -> ! {
    if as_json {
        write_document(&document(
            command,
            false,
            error.payload,
            Some((error.code, &error.message)),
        ));
    } else {
        let mut stderr = std::io::stderr();
        for warning in &error.warnings {
            write_text(&mut stderr, warning);
        }
        let prose = error
            .prose
            .unwrap_or_else(|| format!("loch: {}\n", error.message));
        write_text(&mut stderr, &prose);
    }
    let _ = std::io::stdout().flush();
    std::process::exit(error.exit_status);
}

fn write_document(body: &Value) {
    let mut out = json::dumps(body, json::DEFAULT);
    out.push('\n');
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}
