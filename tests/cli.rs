//! The commands end to end, through the binary: what each prints, what it
//! writes, and the JSON contract a consumer leans on (one document on stdout,
//! failures as documents with a stable `code`, exit status 2 for usage).

use loch::bundle::unpack_bundle;
use loch::claude::{session_dir_for, transcript_cwds, transcript_path_for};
use loch::format;
use loch::json::{self, Map, Value};
use loch::object;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const SECRET_PREFIX: &str = "ghp_";

fn secret() -> String {
    format!("{}{}", SECRET_PREFIX, "a1B2c3D4e5".repeat(4))
}

// -- scratch directories -----------------------------------------------------

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "loch-cli-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        // pytest's tmp_path is symlink-resolved; /var is /private/var on macOS.
        Tmp(fs::canonicalize(&dir).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, part: &str) -> PathBuf {
        self.0.join(part)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn s(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

// -- running the binary ------------------------------------------------------

struct Ran {
    out: String,
    err: String,
    code: i32,
}

const SCRUBBED: &[&str] = &[
    "LOCH_STORE",
    "LOCH_S3_ENDPOINT",
    "LOCH_S3_REGION",
    "CLAUDE_CONFIG_DIR",
    "SOURCE_DATE_EPOCH",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_CONFIG_FILE",
    "AWS_SHARED_CREDENTIALS_FILE",
    "GIT_DIR",
    "GIT_WORK_TREE",
];

fn invoke_env(tmp: &Tmp, home: &Path, env: &[(&str, &str)], argv: &[&str]) -> Ran {
    let git_config = tmp.join(".gitconfig-empty");
    if !git_config.exists() {
        fs::write(&git_config, "").unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_loch"));
    command.args(argv).current_dir(tmp.path());
    for name in SCRUBBED {
        command.env_remove(name);
    }
    command
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", &git_config)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    Ran {
        out: String::from_utf8(output.stdout).unwrap(),
        err: String::from_utf8(output.stderr).unwrap(),
        code: output.status.code().unwrap(),
    }
}

fn invoke(tmp: &Tmp, home: &Path, argv: &[&str]) -> Ran {
    invoke_env(tmp, home, &[], argv)
}

/// Asserts the contract a consumer depends on: stdout is one JSON document
/// and nothing else. Prose leaking from any helper fails the parse.
fn invoke_json_env(
    tmp: &Tmp,
    home: &Path,
    env: &[(&str, &str)],
    argv: &[&str],
) -> (Value, String, i32) {
    let ran = invoke_env(tmp, home, env, argv);
    assert_eq!(ran.out.matches('\n').count(), 1, "stdout: {}", ran.out);
    let document = json::loads(&ran.out).expect("stdout is JSON");
    assert_eq!(document.get("schema"), Some(&Value::int(1)));
    (document, ran.err, ran.code)
}

fn invoke_json(tmp: &Tmp, home: &Path, argv: &[&str]) -> (Value, String, i32) {
    invoke_json_env(tmp, home, &[], argv)
}

// -- reading documents -------------------------------------------------------

fn at<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("no {:?} in {:?}", key, value))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    at(value, key)
        .as_str()
        .unwrap_or_else(|| panic!("{:?} is not a string", key))
}

fn items<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    at(value, key)
        .as_array()
        .unwrap_or_else(|| panic!("{:?} is not an array", key))
}

fn only<'a>(value: &'a Value, key: &str) -> &'a Value {
    let list = items(value, key);
    assert_eq!(list.len(), 1, "{:?}: {:?}", key, list);
    &list[0]
}

fn find_ref(out: &str) -> Option<String> {
    out.lines().find_map(|line| {
        let hex = line.strip_prefix("ref ")?;
        is_ref(hex).then(|| hex.to_string())
    })
}

fn is_ref(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// -- writing sessions --------------------------------------------------------

fn dumps(map: Map) -> String {
    json::dumps(&Value::Object(map), json::DEFAULT)
}

fn write_session(
    home: &Path,
    cwd: &str,
    session_id: &str,
    git_branch: Option<&str>,
    summary: &str,
) -> PathBuf {
    let directory = PathBuf::from(session_dir_for(cwd, Some(&s(home))));
    fs::create_dir_all(&directory).unwrap();
    let mut record = object! {
        "type" => "user",
        "sessionId" => session_id,
        "cwd" => cwd,
        "version" => "1.0.99",
        "message" => object! {"role" => "user", "content" => summary},
    };
    if let Some(branch) = git_branch {
        record.insert("gitBranch".into(), Value::str(branch));
    }
    let path = directory.join(format!("{}.jsonl", session_id));
    fs::write(&path, dumps(record) + "\n").unwrap();
    path
}

/// A session that began in `left` and moved to `filed_under` after `stale`
/// records, filed under the directory it ended in.
fn write_moved_session(
    home: &Path,
    session_id: &str,
    left: &str,
    filed_under: &str,
    stale: usize,
) -> PathBuf {
    let record = |cwd: &str, branch: &str, text: &str| {
        dumps(object! {
            "type" => "user",
            "sessionId" => session_id,
            "cwd" => cwd,
            "gitBranch" => branch,
            "version" => "1.0.99",
            "message" => object! {"role" => "user", "content" => text},
        })
    };
    let directory = PathBuf::from(session_dir_for(filed_under, Some(&s(home))));
    fs::create_dir_all(&directory).unwrap();
    let mut lines: Vec<String> = (0..stale)
        .map(|_| record(left, "main", "started here"))
        .collect();
    lines.push(record(filed_under, "feat/send", "moved here"));
    let path = directory.join(format!("{}.jsonl", session_id));
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn save_two_branches(tmp: &Tmp, home: &Path, project: &Path, store: &Path) -> String {
    fs::create_dir_all(project).unwrap();
    write_session(home, &s(project), "abc", Some("main"), "hello");
    write_session(home, &s(project), "def", Some("feature/foo"), "hello");

    let ran = invoke(
        tmp,
        home,
        &["save", "--cwd", &s(project), "--store", &s(store)],
    );
    let found = find_ref(&ran.out).expect("a ref line");
    assert!(ran.out.contains("indexed under [feature/foo], [main]"));
    found
}

fn wipe_index(store: &Path) {
    fn walk(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path);
            } else {
                fs::remove_file(&path).unwrap();
            }
        }
    }
    walk(&store.join("index"));
}

// -- tests -------------------------------------------------------------------

#[test]
fn test_help_is_printed_without_a_command() {
    let tmp = Tmp::new();
    let ran = invoke(&tmp, tmp.path(), &[]);
    assert!(ran.out.starts_with("loch — save and restore"));
    assert_eq!(ran.code, 0);
}

#[test]
fn test_version_is_reported_in_both_renderings() {
    let tmp = Tmp::new();
    let installed = env!("CARGO_PKG_VERSION");

    let ran = invoke(&tmp, tmp.path(), &["--version"]);
    assert_eq!(
        (ran.out.as_str(), ran.code),
        (format!("loch {}\n", installed).as_str(), 0)
    );

    let (document, _, code) = invoke_json(&tmp, tmp.path(), &["--version", "--json"]);
    assert_eq!(text(&document, "command"), "version");
    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert_eq!(code, 0);
    assert_eq!(text(&document, "version"), installed);
}

// test_version_reports_a_source_checkout_that_was_never_installed: skipped,
// it monkeypatched importlib.metadata; the Rust version is fixed at compile time.

#[test]
fn test_unknown_command_fails() {
    let tmp = Tmp::new();
    let ran = invoke(&tmp, tmp.path(), &["bogus"]);
    assert_eq!(ran.err, "loch: unknown command 'bogus' (try: loch help)\n");
    assert_eq!(ran.code, 1);
}

#[test]
fn test_list_reports_sessions() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    write_session(&home, &s(&project), "abc", Some("main"), "do the thing");

    let ran = invoke(&tmp, &home, &["list", "--cwd", &s(&project)]);

    assert!(ran.out.contains("1 session(s) for"));
    assert!(ran.out.contains("abc"));
    assert!(ran.out.contains("[main] do the thing"));
}

#[test]
fn test_list_reports_nothing_for_an_unknown_cwd() {
    let tmp = Tmp::new();
    let ran = invoke(&tmp, &tmp.join("home"), &["list", "--cwd", &s(tmp.path())]);
    assert!(ran.out.starts_with("no claude-code sessions found for"));
}

#[test]
fn test_save_then_restore_onto_another_machine() {
    let tmp = Tmp::new();
    let origin_home = tmp.join("origin-home");
    let origin_project = tmp.join("origin/proj");
    fs::create_dir_all(&origin_project).unwrap();
    let store = tmp.join("store");

    let transcript = origin_home.join("note.txt");
    write_session(
        &origin_home,
        &s(&origin_project),
        "abc",
        Some("main"),
        "hello",
    );
    let session_path = PathBuf::from(transcript_path_for(
        &s(&origin_project),
        "abc",
        Some(&s(&origin_home)),
    ));
    fs::write(
        &session_path,
        dumps(object! {
            "type" => "user",
            "sessionId" => "abc",
            "cwd" => s(&fs::canonicalize(&origin_project).unwrap()),
            "gitBranch" => "main",
            "version" => "1.0.99",
            "message" => object! {
                "role" => "user",
                "content" => format!("read {}", s(&transcript)),
            },
        }) + "\n",
    )
    .unwrap();

    let ran = invoke(
        &tmp,
        &origin_home,
        &["save", "--cwd", &s(&origin_project), "--store", &s(&store)],
    );
    assert!(ran.out.contains("packed abc [main]"));
    let found = find_ref(&ran.out).expect("a ref line");
    assert!(store.join(format!("bundles/{}.loch", found)).exists());
    assert!(store.join(format!("index/branch/main/{}", found)).exists());

    let target_home = tmp.join("target-home");
    let target_project = tmp.join("target/work/proj");
    fs::create_dir_all(&target_project).unwrap();

    let ran = invoke(
        &tmp,
        &target_home,
        &[
            "restore",
            &found,
            "--into",
            &s(&target_project),
            "--store",
            &s(&store),
        ],
    );
    let resolved_target = s(&fs::canonicalize(&target_project).unwrap());
    assert!(ran.out.contains("restored abc [main]"));
    assert!(ran
        .out
        .contains(&format!("cd {} && claude --resume abc", resolved_target)));

    let restored = transcript_path_for(&s(&target_project), "abc", Some(&s(&target_home)));
    let body = fs::read_to_string(restored).unwrap();
    assert!(body.contains(&resolved_target));
    assert!(!body.contains(&s(&fs::canonicalize(&origin_project).unwrap())));
    assert!(!body.contains(&s(&origin_home)));
    assert!(body.contains(&s(&target_home.join("note.txt"))));
}

#[test]
fn test_save_redacts_secrets_without_touching_the_local_transcript() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    let secret = "AKIAZZZZZZZZEXAMPLE0";
    let session_path = write_session(
        &home,
        &s(&project),
        "abc",
        Some("main"),
        &format!("ran env, saw {}", secret),
    );

    let ran = invoke(
        &tmp,
        &home,
        &["save", "--cwd", &s(&project), "--store", &s(&store)],
    );

    assert!(ran.out.contains("redacted 1 secret (aws-access-key ×1)"));
    assert!(!ran.out.contains(secret));
    assert!(fs::read_to_string(&session_path).unwrap().contains(secret));

    let found = find_ref(&ran.out).expect("a ref line");
    let bundle =
        unpack_bundle(&fs::read(store.join(format!("bundles/{}.loch", found))).unwrap()).unwrap();
    assert!(!bundle.sessions[0].transcript.contains(secret));
    assert!(bundle.sessions[0]
        .transcript
        .contains("[REDACTED:aws-access-key]"));
}

#[test]
fn test_restore_skips_an_existing_session_unless_forced() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    write_session(&home, &s(&project), "abc", Some("main"), "hello");

    let ran = invoke(
        &tmp,
        &home,
        &["save", "--cwd", &s(&project), "--store", &s(&store)],
    );
    let found = find_ref(&ran.out).expect("a ref line");

    let restore = [
        "restore",
        found.as_str(),
        "--into",
        &s(&project),
        "--store",
        &s(&store),
    ];
    let ran = invoke(&tmp, &home, &restore);
    assert!(ran
        .err
        .contains("skipped abc (already exists; --force to overwrite)"));
    assert!(ran.err.ends_with("loch: nothing restored\n"));
    assert_eq!(ran.code, 1);

    let mut forced = restore.to_vec();
    forced.push("--force");
    let ran = invoke(&tmp, &home, &forced);
    assert!(ran.out.contains("restored abc [main]"));
}

#[test]
fn test_restore_with_a_new_id_rewrites_filename_and_records() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    write_session(&home, &s(&project), "abc", Some("main"), "hello");

    let ran = invoke(
        &tmp,
        &home,
        &["save", "--cwd", &s(&project), "--store", &s(&store)],
    );
    let found = find_ref(&ran.out).expect("a ref line");

    invoke(
        &tmp,
        &home,
        &[
            "restore",
            &found,
            "--into",
            &s(&project),
            "--store",
            &s(&store),
            "--new-id",
        ],
    );

    let directory = PathBuf::from(session_dir_for(&s(&project), Some(&s(&home))));
    let minted: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl") && path.file_stem().unwrap() != "abc"
        })
        .collect();
    assert_eq!(minted.len(), 1);
    let body = fs::read_to_string(&minted[0]).unwrap();
    let record = json::loads(body.lines().next().unwrap()).unwrap();
    assert_eq!(
        text(&record, "sessionId"),
        minted[0].file_stem().unwrap().to_str().unwrap()
    );
}

#[test]
fn test_save_fails_when_nothing_matches() {
    let tmp = Tmp::new();
    let ran = invoke(
        &tmp,
        &tmp.join("home"),
        &[
            "save",
            "--cwd",
            &s(tmp.path()),
            "--store",
            &s(&tmp.join("store")),
        ],
    );
    assert!(ran
        .err
        .starts_with("loch: no claude-code sessions found for"));
    assert_eq!(ran.code, 1);
}

#[test]
fn test_restore_requires_a_ref() {
    let tmp = Tmp::new();
    let ran = invoke(&tmp, tmp.path(), &["restore"]);
    assert_eq!(ran.err, "loch: restore requires a bundle ref\n");
    assert_eq!(ran.code, 1);
}

#[test]
fn test_restore_reports_an_unreadable_ref() {
    let tmp = Tmp::new();
    let ran = invoke(
        &tmp,
        tmp.path(),
        &["restore", "deadbeef", "--store", &s(tmp.path())],
    );
    assert!(ran.err.starts_with("loch: could not read deadbeef from"));
    assert_eq!(ran.code, 1);
}

#[test]
fn test_list_remote_finds_a_bundle_by_branch() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    let found = save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    let ran = invoke(
        &tmp,
        &home,
        &[
            "list",
            "--remote",
            "--branch",
            "feature/foo",
            "--store",
            &s(&store),
        ],
    );
    assert!(ran.out.contains("1 bundle(s) for [feature/foo]"));
    assert!(ran.out.contains(&found));
    assert!(ran.out.contains("1 session(s)"));
    assert!(ran.out.contains("claude 1.0.99"));

    let ran = invoke(
        &tmp,
        &home,
        &[
            "list",
            "--remote",
            "--branch",
            "nope",
            "--store",
            &s(&store),
        ],
    );
    assert!(ran.out.starts_with("no bundles for [nope] in"));
}

#[test]
fn test_list_remote_defaults_to_the_branch_you_are_on() {
    // The Python monkeypatched current_branch; here the cwd is a real
    // repository whose HEAD is on feature/foo.
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    let found = save_two_branches(&tmp, &home, &tmp.join("proj"), &store);
    let repo = tmp.join("repo");
    fs::create_dir(&repo).unwrap();
    let git_config = tmp.join(".gitconfig-empty");
    fs::write(&git_config, "").unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "-b", "feature/foo"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", &git_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success());

    let ran = invoke(
        &tmp,
        &home,
        &[
            "list",
            "--remote",
            "--cwd",
            &s(&repo),
            "--store",
            &s(&store),
        ],
    );

    assert!(
        ran.out.contains("1 bundle(s) for [feature/foo]"),
        "{}",
        ran.out
    );
    assert!(ran.out.contains(&found));
}

#[test]
fn test_list_remote_all_shows_every_branch() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    let ran = invoke(&tmp, &home, &["list", "--all", "--store", &s(&store)]);

    assert!(ran.out.starts_with("1 bundle(s) in"));
    assert!(ran.out.contains("[feature/foo]"));
    assert!(ran.out.contains("[main]"));
}

#[test]
fn test_delete_removes_the_bundle_and_its_entries() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    let found = save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    let ran = invoke(&tmp, &home, &["delete", &found, "--store", &s(&store)]);
    assert!(ran.out.contains(&format!("removed bundles/{}.loch", found)));
    assert!(ran
        .out
        .contains(&format!("removed index/branch/feature%2Ffoo/{}", found)));

    let ran = invoke(&tmp, &home, &["list", "--all", "--store", &s(&store)]);
    assert!(ran.out.starts_with("no bundles in"));

    let ran = invoke(&tmp, &home, &["restore", &found, "--store", &s(&store)]);
    assert!(ran
        .err
        .starts_with(&format!("loch: could not read {} from", found)));
    assert_eq!(ran.code, 1);
}

#[test]
fn test_delete_reports_a_ref_that_is_not_there() {
    let tmp = Tmp::new();
    let ran = invoke(
        &tmp,
        tmp.path(),
        &["delete", "deadbeef", "--store", &s(tmp.path())],
    );
    assert!(ran.err.starts_with("loch: could not delete deadbeef from"));
    assert_eq!(ran.code, 1);
}

#[test]
fn test_reindex_rebuilds_a_wiped_index() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    let found = save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    wipe_index(&store);

    let ran = invoke(&tmp, &home, &["reindex", "--store", &s(&store)]);
    assert!(ran.out.contains("scanned 1 bundle(s) in"));
    assert!(ran.out.contains("wrote 2 index entries, removed 0 stale"));

    let ran = invoke(&tmp, &home, &["list", "--all", "--store", &s(&store)]);
    assert!(ran.out.contains(&found));
    assert!(ran.out.contains("[feature/foo]"));
    assert!(ran.out.contains("[main]"));
}

#[test]
fn test_json_list_emits_raw_values_not_formatted_columns() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    write_session(&home, &s(&project), "abc", Some("main"), "do the thing");

    let (document, err, code) =
        invoke_json(&tmp, &home, &["list", "--json", "--cwd", &s(&project)]);

    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert_eq!(text(&document, "command"), "list");
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(at(&document, "remote"), &Value::Bool(false));
    let session = only(&document, "sessions");
    assert_eq!(text(session, "sessionId"), "abc");
    assert_eq!(text(session, "branch"), "main");
    assert!(matches!(at(session, "bytes"), Value::Int(_)));
    let modified = text(session, "modifiedAt");
    let before_z = modified.strip_suffix('Z').expect("ends in Z");
    assert!(before_z.ends_with(|c: char| c.is_ascii_digit()));
}

/// The summary is the only transcript content either rendering prints, and it
/// reaches a picker row and a pasted bug report without a `save` in between.
#[test]
fn test_json_list_carries_a_scrubbed_summary_in_both_renderings() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    write_session(
        &home,
        &s(&project),
        "abc",
        Some("main"),
        &format!("deploy with {}", secret()),
    );

    let (document, _, _) = invoke_json(&tmp, &home, &["list", "--json", "--cwd", &s(&project)]);
    let ran = invoke(&tmp, &home, &["list", "--cwd", &s(&project)]);

    assert_eq!(
        text(&items(&document, "sessions")[0], "summary"),
        "deploy with [REDACTED:github-token]"
    );
    assert!(!ran.out.contains(&secret()));
}

#[test]
fn test_json_list_flag_is_accepted_before_the_command() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    write_session(&home, &s(&project), "abc", Some("main"), "hello");

    let (document, _, _) = invoke_json(&tmp, &home, &["--json", "list", "--cwd", &s(&project)]);

    let ids: Vec<&str> = items(&document, "sessions")
        .iter()
        .map(|session| text(session, "sessionId"))
        .collect();
    assert_eq!(ids, ["abc"]);
}

#[test]
fn test_json_list_finding_nothing_is_an_empty_result_not_an_error() {
    let tmp = Tmp::new();
    let (document, _, code) = invoke_json(
        &tmp,
        &tmp.join("home"),
        &["list", "--json", "--cwd", &s(tmp.path())],
    );

    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert!(items(&document, "sessions").is_empty());
    assert_eq!(code, 0);
}

#[test]
fn test_json_list_remote_finding_nothing_is_an_empty_result_not_an_error() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    let (document, _, code) = invoke_json(
        &tmp,
        &home,
        &[
            "list",
            "--json",
            "--remote",
            "--branch",
            "nope",
            "--store",
            &s(&store),
        ],
    );

    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert!(items(&document, "entries").is_empty());
    assert_eq!(at(&document, "bundles"), &Value::int(0));
    assert_eq!(code, 0);
}

#[test]
fn test_json_list_remote_reports_entries_by_dimension() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    let found = save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    let (document, _, _) = invoke_json(
        &tmp,
        &home,
        &["list", "--json", "--all", "--store", &s(&store)],
    );

    assert_eq!(at(&document, "remote"), &Value::Bool(true));
    assert_eq!(at(&document, "all"), &Value::Bool(true));
    assert_eq!(at(&document, "bundles"), &Value::int(1));
    let entries = items(&document, "entries");
    let pairs: BTreeSet<(&str, &str)> = entries
        .iter()
        .map(|entry| (text(entry, "dimension"), text(entry, "value")))
        .collect();
    assert_eq!(
        pairs,
        BTreeSet::from([("branch", "main"), ("branch", "feature/foo")])
    );
    assert!(entries.iter().all(|entry| text(entry, "ref") == found));
    assert!(entries
        .iter()
        .all(|entry| matches!(at(entry, "bytes"), Value::Int(_))));
}

#[test]
fn test_json_save_reports_the_ref_and_redaction_counts_without_the_match() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    let secret = "AKIAZZZZZZZZEXAMPLE0";
    write_session(
        &home,
        &s(&project),
        "abc",
        Some("main"),
        &format!("ran env, saw {}", secret),
    );

    let (document, _, _) = invoke_json(
        &tmp,
        &home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&project),
            "--store",
            &s(&store),
        ],
    );

    let found = text(&document, "ref");
    assert!(is_ref(found));
    let size = fs::metadata(store.join(format!("bundles/{}.loch", found)))
        .unwrap()
        .len();
    assert_eq!(at(&document, "bytes"), &Value::int(size as i64));
    assert_eq!(text(&document, "store"), s(&store));
    assert_eq!(at(&document, "redacted"), &Value::int(1));
    let session = only(&document, "sessions");
    assert_eq!(text(session, "sessionId"), "abc");
    assert_eq!(text(session, "branch"), "main");
    assert_eq!(
        at(session, "redactions"),
        &Value::Object(object! {"aws-access-key" => 1i64})
    );
    assert_eq!(
        at(&document, "indexed"),
        &Value::Array(vec![Value::Object(object! {
            "dimension" => "branch",
            "value" => "main",
            "key" => format!("index/branch/main/{}", found),
        })])
    );
    assert!(!json::dumps(&document, json::DEFAULT).contains(secret));
}

#[test]
fn test_json_save_reports_every_branch_it_indexed() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    write_session(&home, &s(&project), "abc", Some("main"), "hello");
    write_session(&home, &s(&project), "def", Some("feature/foo"), "hello");

    let (document, _, _) = invoke_json(
        &tmp,
        &home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&project),
            "--store",
            &s(&store),
        ],
    );

    let values: BTreeSet<&str> = items(&document, "indexed")
        .iter()
        .map(|entry| text(entry, "value"))
        .collect();
    assert_eq!(values, BTreeSet::from(["main", "feature/foo"]));
    let ids: BTreeSet<&str> = items(&document, "sessions")
        .iter()
        .map(|session| text(session, "sessionId"))
        .collect();
    assert_eq!(ids, BTreeSet::from(["abc", "def"]));
    assert_eq!(at(&document, "redacted"), &Value::int(0));
}

#[test]
fn test_json_restore_reports_per_session_status_and_resume_commands() {
    let tmp = Tmp::new();
    let origin_home = tmp.join("origin-home");
    let origin_project = tmp.join("origin/proj");
    fs::create_dir_all(&origin_project).unwrap();
    let store = tmp.join("store");
    write_session(
        &origin_home,
        &s(&origin_project),
        "abc",
        Some("main"),
        "hello",
    );

    let (saved, _, _) = invoke_json(
        &tmp,
        &origin_home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&origin_project),
            "--store",
            &s(&store),
        ],
    );

    let target_home = tmp.join("target-home");
    let target_project = tmp.join("target/proj");
    fs::create_dir_all(&target_project).unwrap();

    let (document, err, code) = invoke_json(
        &tmp,
        &target_home,
        &[
            "restore",
            "--json",
            text(&saved, "ref"),
            "--into",
            &s(&target_project),
            "--store",
            &s(&store),
        ],
    );

    let resolved_target = s(&fs::canonicalize(&target_project).unwrap());
    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(text(&document, "cwd"), resolved_target);
    assert_eq!(text(at(&document, "origin"), "home"), s(&origin_home));
    let session = only(&document, "sessions");
    assert_eq!(text(session, "status"), "restored");
    assert_eq!(text(session, "sessionId"), "abc");
    assert_eq!(text(session, "originSessionId"), "abc");
    assert!(items(session, "residualOriginPaths").is_empty());
    assert_eq!(
        text(session, "resumeCommand"),
        format!("cd {} && claude --resume abc", resolved_target)
    );
    assert!(!fs::read_to_string(text(session, "path"))
        .unwrap()
        .is_empty());
}

#[test]
fn test_json_restore_reports_residual_origin_paths() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    write_session(&home, &s(&project), "abc", Some("main"), "hello");

    let (saved, _, _) = invoke_json(
        &tmp,
        &home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&project),
            "--store",
            &s(&store),
        ],
    );

    // Restoring below the origin cwd leaves it a substring of the target, so
    // the rewritten transcript still mentions a path from the origin machine.
    let nested = project.join("nested");
    fs::create_dir(&nested).unwrap();
    let (document, _, _) = invoke_json(
        &tmp,
        &home,
        &[
            "restore",
            "--json",
            text(&saved, "ref"),
            "--into",
            &s(&nested),
            "--store",
            &s(&store),
        ],
    );

    let session = only(&document, "sessions");
    assert_eq!(
        at(session, "residualOriginPaths"),
        &Value::Array(vec![Value::str(s(&fs::canonicalize(&project).unwrap()))])
    );
}

/// The whole failure, end to end. A session that began in the main repo and
/// moved into a worktree used to be saved under the repo it had left, which
/// keyed the rewrite on the wrong cwd: every worktree record then declined the
/// cwd pair, fell through to the home pair, and landed on a well-formed path
/// that exists nowhere. residualOriginPaths stayed empty throughout, which is
/// why foreignCwds is the field a caller can gate on.
#[test]
fn test_a_moved_session_is_saved_and_restored_from_the_cwd_it_ended_in() {
    let tmp = Tmp::new();
    let origin_home = tmp.join("origin-home");
    let main = origin_home.join("apps/harness");
    let worktree = origin_home.join("apps/harness-worktrees/feat/send");
    fs::create_dir_all(&worktree).unwrap();
    fs::create_dir_all(&main).unwrap();
    let store = tmp.join("store");
    write_moved_session(&origin_home, "abc", &s(&main), &s(&worktree), 300);

    let (saved, _, _) = invoke_json(
        &tmp,
        &origin_home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&worktree),
            "--store",
            &s(&store),
        ],
    );

    let packed = only(&saved, "sessions");
    assert_eq!(text(packed, "cwd"), s(&worktree));
    assert_eq!(text(packed, "branch"), "feat/send");
    let indexed: Vec<&str> = items(&saved, "indexed")
        .iter()
        .map(|entry| text(entry, "value"))
        .collect();
    assert_eq!(indexed, ["feat/send"]);

    let target_home = tmp.join("target-home");
    let target = target_home.join("apps/harness-worktrees/feat/send");
    fs::create_dir_all(&target).unwrap();

    let (document, err, _) = invoke_json(
        &tmp,
        &target_home,
        &[
            "restore",
            "--json",
            text(&saved, "ref"),
            "--into",
            &s(&target),
            "--store",
            &s(&store),
        ],
    );

    let session = only(&document, "sessions");
    assert_eq!(text(session, "status"), "restored");
    assert_eq!(err, "");

    let restored = fs::read_to_string(text(session, "path")).unwrap();
    let cwds = transcript_cwds(&restored);
    let target_count = cwds
        .iter()
        .find(|(cwd, _)| *cwd == s(&target))
        .map(|(_, count)| *count);
    assert_eq!(target_count, Some(1));
    // The records from before the move name a genuinely different directory, so
    // no single target cwd can satisfy them. They are reported, not hidden.
    assert!(items(session, "residualOriginPaths").is_empty());
    let mut expected = Map::new();
    expected.insert(
        s(&main),
        Value::Object(object! {
            "count" => 300i64,
            "restored" => s(&target_home.join("apps/harness")),
        }),
    );
    assert_eq!(at(session, "foreignCwds"), &Value::Object(expected));
}

#[test]
fn test_json_restore_reports_a_skip_as_status_not_as_stderr_prose() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let store = tmp.join("store");
    write_session(&home, &s(&project), "abc", Some("main"), "hello");

    let (saved, _, _) = invoke_json(
        &tmp,
        &home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&project),
            "--store",
            &s(&store),
        ],
    );

    let (document, err, code) = invoke_json(
        &tmp,
        &home,
        &[
            "restore",
            "--json",
            text(&saved, "ref"),
            "--into",
            &s(&project),
            "--store",
            &s(&store),
        ],
    );

    assert_eq!(at(&document, "ok"), &Value::Bool(false));
    assert_eq!(text(&document, "code"), "nothing-restored");
    assert_eq!(code, 1);
    assert_eq!(err, "");
    let session = only(&document, "sessions");
    assert_eq!(text(session, "status"), "skipped");
    assert_eq!(at(session, "resumeCommand"), &Value::Null);
    assert_eq!(
        text(session, "path"),
        transcript_path_for(&s(&project), "abc", Some(&s(&home)))
    );
}

#[test]
fn test_json_delete_reports_the_keys_it_removed() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    let found = save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    let (document, _, code) = invoke_json(
        &tmp,
        &home,
        &["delete", "--json", &found, "--store", &s(&store)],
    );

    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert_eq!(code, 0);
    assert_eq!(text(&document, "ref"), found);
    let removed: BTreeSet<&str> = items(&document, "removed")
        .iter()
        .map(|key| key.as_str().unwrap())
        .collect();
    let expected = [
        format!("bundles/{}.loch", found),
        format!("index/branch/main/{}", found),
        format!("index/branch/feature%2Ffoo/{}", found),
    ];
    assert_eq!(
        removed,
        expected.iter().map(String::as_str).collect::<BTreeSet<_>>()
    );
}

#[test]
fn test_json_reindex_reports_what_it_scanned_and_wrote() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let store = tmp.join("store");
    save_two_branches(&tmp, &home, &tmp.join("proj"), &store);

    wipe_index(&store);

    let (document, _, _) = invoke_json(&tmp, &home, &["reindex", "--json", "--store", &s(&store)]);

    assert_eq!(at(&document, "bundles"), &Value::int(1));
    assert_eq!(at(&document, "entries"), &Value::int(2));
    assert_eq!(at(&document, "removed"), &Value::int(0));
    assert_eq!(text(&document, "store"), s(&store));
}

#[test]
fn test_json_failure_is_still_one_parseable_document_on_stdout() {
    let tmp = Tmp::new();
    let (document, err, code) = invoke_json(
        &tmp,
        &tmp.join("home"),
        &[
            "save",
            "--json",
            "--cwd",
            &s(tmp.path()),
            "--store",
            &s(&tmp.join("store")),
        ],
    );

    assert_eq!(at(&document, "ok"), &Value::Bool(false));
    assert_eq!(text(&document, "code"), "no-sessions");
    assert_eq!(code, 1);
    assert!(text(&document, "error").starts_with("no claude-code sessions found for"));
    assert_eq!(err, "");
}

/// The distinction a consumer acts on: an absent ref is terminal, an
/// unreachable store is worth retrying.
#[test]
fn test_json_separates_a_ref_that_is_absent_from_a_store_that_did_not_answer() {
    let tmp = Tmp::new();
    let (document, _, code) = invoke_json(&tmp, tmp.path(), &["restore", "--json"]);
    assert_eq!((text(&document, "code"), code), ("missing-ref", 1));

    for command in ["restore", "delete"] {
        let (document, _, code) = invoke_json(
            &tmp,
            tmp.path(),
            &[
                command,
                "--json",
                "deadbeef",
                "--store",
                &s(&tmp.join("store")),
            ],
        );
        assert_eq!((text(&document, "code"), code), ("bundle-not-found", 1));
    }

    // A file where the store's directory should be: the store cannot answer at
    // all, which is a different failure from the object being absent.
    let blocked = tmp.join("blocked");
    fs::write(&blocked, "not a store").unwrap();
    let (document, _, code) = invoke_json(
        &tmp,
        tmp.path(),
        &["restore", "--json", "deadbeef", "--store", &s(&blocked)],
    );
    assert_eq!((text(&document, "code"), code), ("store-unreachable", 1));
}

/// A default install has no boto3, and telling a caller to retry a network
/// problem when the fix is an install is the wrong affordance. The Rust build
/// carries its own S3 client, so `s3-extra-missing` cannot arise and that half
/// is skipped; the other half, a store that doesn't answer, is checked against
/// a port nothing listens on.
#[test]
fn test_json_separates_an_uninstalled_sdk_from_a_store_that_did_not_answer() {
    let tmp = Tmp::new();
    let (document, _, code) = invoke_json_env(
        &tmp,
        tmp.path(),
        &[
            ("LOCH_S3_ENDPOINT", "http://127.0.0.1:1"),
            ("LOCH_S3_REGION", "us-east-1"),
            ("AWS_ACCESS_KEY_ID", "testing"),
            ("AWS_SECRET_ACCESS_KEY", "testing"),
        ],
        &[
            "list",
            "--json",
            "--remote",
            "--store",
            "s3://sessions/prefix",
        ],
    );
    assert_eq!((text(&document, "code"), code), ("store-unreachable", 1));
}

#[test]
fn test_json_reports_a_bundle_that_arrived_but_will_not_unpack() {
    let tmp = Tmp::new();
    let store = tmp.join("store");
    let bundles = store.join("bundles");
    fs::create_dir_all(&bundles).unwrap();
    fs::write(bundles.join("deadbeef.loch"), b"not a gzip stream").unwrap();

    let (document, _, code) = invoke_json(
        &tmp,
        tmp.path(),
        &["restore", "--json", "deadbeef", "--store", &s(&store)],
    );

    assert_eq!((text(&document, "code"), code), ("bundle-unreadable", 1));
}

#[test]
fn test_json_reports_an_unknown_command_and_a_bad_flag() {
    let tmp = Tmp::new();
    let (document, err, code) = invoke_json(&tmp, tmp.path(), &["--json", "bogus"]);
    assert_eq!(
        (text(&document, "command"), text(&document, "code"), code),
        ("bogus", "unknown-command", 1)
    );
    assert_eq!(err, "");

    let (document, err, code) = invoke_json(&tmp, tmp.path(), &["list", "--json", "--bogus"]);
    assert_eq!(
        (text(&document, "command"), text(&document, "code"), code),
        ("list", "usage", 2)
    );
    assert_eq!(err, "");
}

#[test]
fn test_json_help_is_a_document_rather_than_the_usage_block() {
    let tmp = Tmp::new();
    let (document, _, code) = invoke_json(&tmp, tmp.path(), &["--json"]);
    assert_eq!(text(&document, "command"), "help");
    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert_eq!(code, 0);
    assert!(text(&document, "usage").starts_with("loch — save and restore"));

    let (document, _, code) = invoke_json(&tmp, tmp.path(), &["save", "--json", "--help"]);
    assert_eq!(text(&document, "command"), "save");
    assert_eq!(at(&document, "ok"), &Value::Bool(true));
    assert_eq!(code, 0);
    assert!(text(&document, "usage").contains("--store"));
}

#[test]
fn test_ingest_writes_a_verified_derived_file_per_session() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let out_dir = tmp.join("derived");
    let path = PathBuf::from(transcript_path_for(&s(&project), "abc", Some(&s(&home))));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        dumps(object! {
            "type" => "user",
            "uuid" => "u-1",
            "parentUuid" => Value::Null,
            "sessionId" => "abc",
            "cwd" => s(&fs::canonicalize(&project).unwrap()),
            "message" => object! {"role" => "user", "content" => "hello"},
        }) + "\n",
    )
    .unwrap();

    let (document, err, code) = invoke_json(
        &tmp,
        &home,
        &[
            "ingest",
            "--json",
            "--cwd",
            &s(&project),
            "--author",
            "alice",
            "--out",
            &s(&out_dir),
        ],
    );

    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        at(&document, "converter"),
        &Value::Object(object! {"id" => "claude-code", "version" => 1i64})
    );
    let entry = only(&document, "sessions");
    assert_eq!(at(entry, "turns"), &Value::int(1));

    // There is no read_derived in the Rust build, so the verification it did
    // is done here: every turn id is recomputed from its core.
    let body = fs::read_to_string(text(entry, "path")).unwrap();
    let records: Vec<Value> = body
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| json::loads(line).unwrap())
        .collect();
    let header = records
        .iter()
        .find(|record| at(record, "type").is_str("header"))
        .expect("a header");
    assert_eq!(
        at(header, "author"),
        &Value::Object(object! {"kind" => "human", "id" => "alice"})
    );
    let turns: Vec<&Value> = records
        .iter()
        .filter(|record| at(record, "type").is_str("turn"))
        .collect();
    assert_eq!(turns.len(), 1);
    for turn in &turns {
        assert_eq!(format::digest(at(turn, "turn")).unwrap(), text(turn, "id"));
    }
    assert_eq!(text(at(entry, "heads"), "current"), text(turns[0], "id"));
}

#[test]
fn test_ingest_fails_when_nothing_matches() {
    let tmp = Tmp::new();
    let (document, _, code) = invoke_json(
        &tmp,
        &tmp.join("home"),
        &[
            "ingest",
            "--json",
            "--cwd",
            &s(tmp.path()),
            "--out",
            &s(&tmp.join("derived")),
        ],
    );

    assert_eq!((code, text(&document, "code")), (1, "no-sessions"));
}

#[test]
fn test_ingest_rejects_an_unreadable_author_key() {
    let tmp = Tmp::new();
    let (document, _, code) = invoke_json(
        &tmp,
        &tmp.join("home"),
        &[
            "ingest",
            "--json",
            "--author-key",
            &s(&tmp.join("missing.pub")),
        ],
    );

    assert_eq!(
        (code, text(&document, "code")),
        (1, "author-key-unreadable")
    );
}

#[test]
fn test_an_undecodable_transcript_is_listed_but_not_packed() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    let path = write_session(&home, &s(&project), "abc", Some("main"), "hello");
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"type\": \"user\", \"x\": \"\xff\"}\n");
    fs::write(&path, bytes).unwrap();

    let (listed, _, _) = invoke_json(&tmp, &home, &["list", "--json", "--cwd", &s(&project)]);
    let ids: Vec<&str> = items(&listed, "sessions")
        .iter()
        .map(|session| text(session, "sessionId"))
        .collect();
    assert_eq!(ids, ["abc"]);

    let store = tmp.join("store");
    let (saved, _, code) = invoke_json(
        &tmp,
        &home,
        &[
            "save",
            "--json",
            "--cwd",
            &s(&project),
            "--store",
            &s(&store),
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(text(&saved, "code"), "transcript-unreadable");
    assert!(!store.exists());
}

#[test]
fn test_source_date_epoch_makes_save_reproducible() {
    let tmp = Tmp::new();
    let home = tmp.join("home");
    let project = tmp.join("proj");
    fs::create_dir(&project).unwrap();
    write_session(&home, &s(&project), "abc", Some("main"), "hello");
    let mut refs = Vec::new();
    for store in ["a", "b"] {
        let (document, _, _) = invoke_json_env(
            &tmp,
            &home,
            &[("SOURCE_DATE_EPOCH", "1700000000")],
            &[
                "save",
                "--json",
                "--cwd",
                &s(&project),
                "--store",
                &s(&tmp.join(store)),
            ],
        );
        assert_eq!(text(&document, "createdAt"), "2023-11-14T22:13:20.000Z");
        refs.push(text(&document, "ref").to_string());
    }
    assert_eq!(refs[0], refs[1]);
}
