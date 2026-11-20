//! `loch remote` and the config it keeps, in a scratch HOME.

use loch::json::{self, Value};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Home(PathBuf);

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn home() -> Home {
    let path = std::env::temp_dir().join(format!(
        "loch-test-remote-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    Home(path)
}

fn loch(home: &Home, args: &[&str]) -> (Value, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_loch"))
        .args(args)
        .arg("--json")
        .env("HOME", &home.0)
        .env_remove("LOCH_STORE")
        .env_remove("LOCH_REMOTE")
        .output()
        .unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    (doc, out.status.code().unwrap())
}

fn code(doc: &Value) -> &str {
    doc.get("code").and_then(Value::as_str).unwrap_or("")
}

fn names(doc: &Value) -> Vec<String> {
    doc.get("remotes")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.get("name").unwrap().as_str().unwrap().to_string())
        .collect()
}

#[test]
fn remotes_are_added_listed_and_removed_in_order() {
    let h = home();
    let (empty, status) = loch(&h, &["remote"]);
    assert_eq!(status, 0);
    assert!(names(&empty).is_empty());
    assert_eq!(empty.get("default"), Some(&Value::Null));

    let (added, _) = loch(&h, &["remote", "add", "backup", "/tmp/loch-backup"]);
    assert!(added.get("ok").unwrap().truthy());
    assert!(added.get("command").unwrap().is_str("remote"));
    // The only remote is the default.
    let (one, _) = loch(&h, &["remote", "list"]);
    assert!(one.get("default").unwrap().is_str("backup"));

    loch(&h, &["remote", "add", "origin", "s3://bucket/prefix"]);
    let (two, _) = loch(&h, &["remote", "list"]);
    assert_eq!(names(&two), vec!["backup", "origin"]);
    assert!(two.get("default").unwrap().is_str("origin"));

    let (removed, _) = loch(&h, &["remote", "remove", "origin"]);
    assert!(removed
        .get("remote")
        .unwrap()
        .get("uri")
        .unwrap()
        .is_str("s3://bucket/prefix"));
    let (left, _) = loch(&h, &["remote", "list"]);
    assert_eq!(names(&left), vec!["backup"]);
}

#[test]
fn remote_failures_have_codes() {
    let h = home();
    loch(&h, &["remote", "add", "origin", "/tmp/x"]);
    let (exists, status) = loch(&h, &["remote", "add", "origin", "/tmp/y"]);
    assert_eq!((code(&exists), status), ("remote-exists", 1));
    let (unknown, _) = loch(&h, &["remote", "remove", "nope"]);
    assert_eq!(code(&unknown), "unknown-remote");
    let (bad_name, status) = loch(&h, &["remote", "add", "a/b", "/tmp/z"]);
    assert_eq!((code(&bad_name), status), ("usage", 2));
    let (bad_uri, _) = loch(&h, &["remote", "add", "web", "http://example.com"]);
    assert_eq!(code(&bad_uri), "invalid-uri");
    let (bad_action, status) = loch(&h, &["remote", "rename"]);
    assert_eq!((code(&bad_action), status), ("usage", 2));
    let (missing, status) = loch(&h, &["remote", "add", "only-a-name"]);
    assert_eq!((code(&missing), status), ("usage", 2));
}

#[test]
fn a_config_that_will_not_parse_is_never_overwritten() {
    let h = home();
    fs::create_dir_all(h.0.join(".loch")).unwrap();
    fs::write(h.0.join(".loch/config"), "not json").unwrap();
    let (listed, _) = loch(&h, &["remote"]);
    assert_eq!(code(&listed), "config-unreadable");
    let (added, _) = loch(&h, &["remote", "add", "origin", "/tmp/x"]);
    assert_eq!(code(&added), "config-unreadable");
    assert_eq!(
        fs::read_to_string(h.0.join(".loch/config")).unwrap(),
        "not json"
    );
}

#[test]
fn a_relative_path_is_kept_absolute_and_an_unknown_scheme_refused() {
    let h = home();
    let out = Command::new(env!("CARGO_BIN_EXE_loch"))
        .args(["remote", "add", "here", "rel/store", "--json"])
        .current_dir(&h.0)
        .env("HOME", &h.0)
        .output()
        .unwrap();
    let added = json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    let uri = added
        .get("remote")
        .unwrap()
        .get("uri")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        uri.starts_with('/') && uri.ends_with("/rel/store"),
        "{}",
        uri
    );
    let (ssh, _) = loch(&h, &["remote", "add", "box", "ssh://host/path"]);
    assert_eq!(code(&ssh), "invalid-uri");
}
