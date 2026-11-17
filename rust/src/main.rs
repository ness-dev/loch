//! The Rust build of loch: one static binary for Ness, the VS Code extension
//! and the Claude Code hooks to ship. The Python package is the reference
//! implementation; a command lands here only once `test/test_equivalence.py`
//! holds it to the same output and side effects.

// The error path is cold and runs once per process; boxing it buys nothing.
#![allow(clippy::result_large_err)]

mod claude;
mod git;
mod hook;
mod output;
mod s3;
mod store;

use output::{emit, emit_error, CommandError, Outcome, JSON_FLAG};
use serde_json::Value;
use std::io::Read;

/// Commands the Python implementation has and this one doesn't yet. Asking
/// for one is `not-ported` rather than `unknown-command`: the command exists,
/// and the fix is the Python install, not a typo.
const UNPORTED: &[&str] = &[
    "list", "save", "restore", "delete", "reindex", "ingest", "install", "blame", "ask", "log",
    "backfill",
];

const USAGE: &str = "loch — save and restore agent coding sessions across machines

This is the Rust build, which so far implements:

  loch hook    pre-tool | post-tool | stop      (run by Claude Code)
  loch --version

Every other command is still served by the Python implementation (`uv tool
install loch`); asking this build for one fails with code `not-ported`.

Every command takes --json, which replaces the text below with one machine-
readable document on stdout.

Environment:
  LOCH_STORE        default store URI
  CLAUDE_CONFIG_DIR Claude Code's state directory (default ~/.claude)
";

fn command_hook(argv: &[String]) -> Result<Outcome, CommandError> {
    let name = argv.first().map(String::as_str).unwrap_or("");
    let mut stdin = Vec::new();
    if std::io::stdin().read_to_end(&mut stdin).is_ok() {
        hook::run(name, &stdin);
    }
    Ok(Outcome::new("hook"))
}

fn command_version(_argv: &[String]) -> Result<Outcome, CommandError> {
    let version = env!("CARGO_PKG_VERSION");
    let mut outcome = Outcome::new("version");
    outcome
        .payload
        .insert("version".into(), Value::from(version));
    outcome.text = format!("loch {}\n", version);
    Ok(outcome)
}

fn command_help(_argv: &[String]) -> Result<Outcome, CommandError> {
    let mut outcome = Outcome::new("help");
    outcome.payload.insert("usage".into(), Value::from(USAGE));
    outcome.text = USAGE.into();
    Ok(outcome)
}

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // Read off argv rather than from a parsed command line: an unknown
    // command has to render in the mode the caller asked for.
    let as_json = argv.iter().any(|arg| arg == JSON_FLAG);
    if argv.first().map(String::as_str) == Some(JSON_FLAG) {
        argv.remove(0);
    }
    let mut command = argv.first().cloned().unwrap_or_else(|| "help".into());
    if command == "--help" || command == "-h" {
        command = "help".into();
    }
    if command == "--version" || command == "-V" {
        command = "version".into();
    }
    let rest = if argv.is_empty() {
        &argv[..]
    } else {
        &argv[1..]
    };

    let result = match command.as_str() {
        "hook" => command_hook(rest),
        "version" => command_version(rest),
        "help" => command_help(rest),
        name if UNPORTED.contains(&name) => Err(CommandError::new(
            "not-ported",
            format!(
                "'{}' is not in the Rust build yet; use the Python loch",
                name
            ),
        )),
        name => Err(CommandError::new(
            "unknown-command",
            format!("unknown command '{}' (try: loch help)", name),
        )),
    };
    match result {
        Ok(outcome) => emit(outcome, as_json),
        Err(error) => emit_error(&command, error, as_json),
    }
}
