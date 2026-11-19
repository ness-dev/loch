//! `emit` and `emit_error` write to the process's real stdout and stderr, and
//! `emit_error` exits, so those tests re-run this test binary as a child that
//! performs the one call, and inspect what the child wrote.

use loch::json::{self, Map, Value};
use loch::output::{document, emit, emit_error, CommandError, Outcome, SCHEMA};
use std::io::Write;
use std::process::Command;

const CHILD: &str = "LOCH_OUTPUT_TEST_CHILD";
/// Written by the child before the call under test, so the test harness's own
/// stdout chatter can be cut away.
const MARK: &str = "\u{1}LOCH-OUTPUT-BEGIN\u{1}";

struct Child {
    status: i32,
    out: String,
    err: String,
}

/// Runs `test` again in a child; returns `None` when already in the child.
fn in_child(test: &str, action: impl FnOnce()) -> Option<Child> {
    if std::env::var(CHILD).as_deref() == Ok(test) {
        let mut stdout = std::io::stdout();
        stdout.write_all(MARK.as_bytes()).unwrap();
        stdout.flush().unwrap();
        action();
        std::process::exit(0);
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (_, out) = stdout
        .split_once(MARK)
        .unwrap_or_else(|| panic!("child never ran: {stdout:?}"));
    Some(Child {
        status: output.status.code().unwrap(),
        out: out.to_string(),
        err: String::from_utf8(output.stderr).unwrap(),
    })
}

fn payload(pairs: &[(&str, Value)]) -> Map {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn outcome(text: &str) -> Outcome {
    Outcome {
        warnings: vec!["warned\n".into()],
        ..Outcome::with(
            "list",
            payload(&[("sessions", Value::Array(vec![]))]),
            text.into(),
        )
    }
}

#[test]
fn a_success_envelope_carries_no_error_fields() {
    let body = document("save", true, payload(&[("ref", Value::str("abc"))]), None);
    assert_eq!(
        body,
        Value::Object(payload(&[
            ("schema", Value::int(SCHEMA)),
            ("ok", Value::Bool(true)),
            ("command", Value::str("save")),
            ("ref", Value::str("abc")),
        ]))
    );
}

#[test]
fn a_failure_envelope_carries_the_stable_code() {
    let body = document(
        "restore",
        false,
        payload(&[("ref", Value::str("abc"))]),
        Some(("nothing-restored", "nothing")),
    );
    assert_eq!(body.get("ok"), Some(&Value::Bool(false)));
    assert_eq!(body.get("code"), Some(&Value::str("nothing-restored")));
    assert_eq!(body.get("error"), Some(&Value::str("nothing")));
    assert_eq!(body.get("ref"), Some(&Value::str("abc")));
}

#[test]
fn json_mode_writes_one_document_and_leaves_stderr_alone() {
    let Some(child) = in_child(
        "json_mode_writes_one_document_and_leaves_stderr_alone",
        || emit(outcome("prose"), true),
    ) else {
        return;
    };

    let doc = json::loads(&child.out).unwrap();
    assert_eq!(doc.get("sessions"), Some(&Value::Array(vec![])));
    assert_eq!(child.out.matches('\n').count(), 1);
    assert_eq!(child.err, "");
}

#[test]
fn text_mode_writes_prose_and_warnings_to_their_own_streams() {
    let Some(child) = in_child(
        "text_mode_writes_prose_and_warnings_to_their_own_streams",
        || emit(outcome("prose\n"), false),
    ) else {
        return;
    };

    assert_eq!(child.out, "prose\n");
    assert_eq!(child.err, "warned\n");
}

fn usage_error() -> CommandError {
    CommandError {
        exit_status: 2,
        ..CommandError::new("usage", "bad flag")
    }
}

#[test]
fn an_error_exits_nonzero_in_both_modes() {
    let Some(child) = in_child("an_error_exits_nonzero_in_both_modes", || {
        let as_json = std::env::var("LOCH_OUTPUT_TEST_JSON").is_ok();
        emit_error("list", usage_error(), as_json)
    }) else {
        return;
    };
    // The first run above is text mode; JSON mode needs its own child.
    assert_eq!(child.status, 2);
    assert_eq!(child.out, "");
    assert_eq!(child.err, "loch: bad flag\n");

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "an_error_exits_nonzero_in_both_modes",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "an_error_exits_nonzero_in_both_modes")
        .env("LOCH_OUTPUT_TEST_JSON", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (_, out) = stdout.split_once(MARK).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        json::loads(out).unwrap().get("code"),
        Some(&Value::str("usage"))
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "");
}

#[test]
fn a_payload_cannot_overwrite_the_envelope() {
    // install's `command` (the hooks' loch path) once replaced the envelope's
    // `command`, so `loch install --json` reported "command": "loch".
    let payload: Map = [("command".to_string(), Value::str("/opt/loch"))]
        .into_iter()
        .collect();
    let body = document("install", true, payload, None);
    assert_eq!(body.get("command"), Some(&Value::str("install")));
}
