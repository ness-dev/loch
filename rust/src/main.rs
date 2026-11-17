//! The Rust build of loch: one static binary for Ness, the VS Code extension
//! and the Claude Code hooks to ship. The Python package is the reference;
//! `test/test_equivalence.py` holds every command here to its output and side
//! effects.

// The error path is cold and runs once per process; boxing it buys nothing.
#![allow(clippy::result_large_err)]

mod argparse;
mod blame_html;
mod bundle;
mod claude;
mod cli;
mod format;
mod git;
mod hook;
mod index;
mod ingest;
mod json;
mod output;
mod py;
mod redact;
mod rewrite;
mod s3;
mod store;
mod sys;
mod trace;

use cli::Failure;
use output::{emit, emit_error, CommandError, Outcome, JSON_FLAG};

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // Read off argv rather than from a parsed command line: an unknown
    // command, or a parser rejecting a flag, has to render in the mode the
    // caller asked for, and neither has a namespace by the time it fails.
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

    let result = match cli::dispatch(&command, rest) {
        Some(result) => result,
        None => Err(Failure::Error(CommandError::new(
            "unknown-command",
            format!("unknown command '{}' (try: loch help)", command),
        ))),
    };
    match result {
        Ok(outcome) => emit(outcome, as_json),
        Err(Failure::Help(text)) => {
            let mut outcome = Outcome::new(&command);
            outcome.payload.insert("usage".into(), text.as_str().into());
            outcome.text = text;
            emit(outcome, as_json);
        }
        Err(Failure::Error(error)) => emit_error(&command, error, as_json),
        // Python prints a traceback here, which is the one thing a consumer
        // can't parse; JSON mode gets the same `internal` document.
        Err(Failure::Internal(message)) => {
            let mut error = CommandError::new("internal", message.clone());
            if !as_json {
                error.prose = Some(format!("loch: internal error: {}\n", message));
            }
            emit_error(&command, error, as_json)
        }
    }
}
