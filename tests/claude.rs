//! Ported from test/test_claude.py.

use loch::claude::{
    encode_cwd, encode_path, list_sessions, projects_dir, read_session_meta, resolve_cwd,
    session_dir_for, transcript_cwds, transcript_path_for, SUMMARY_LIMIT,
};
use loch::json::{self, Value};
use loch::object;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Tests that touch process-wide environment variables hold this.
static ENV: Mutex<()> = Mutex::new(());

fn secret() -> String {
    format!("ghp_{}", "a1B2c3D4e5".repeat(4))
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("loch-test-claude-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(path: &Path) -> String {
    path.to_str().unwrap().to_string()
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
        record.insert("gitBranch".into(), branch.into());
    }
    let path = directory.join(format!("{}.jsonl", session_id));
    fs::write(
        &path,
        json::dumps(&Value::Object(record), json::DEFAULT) + "\n",
    )
    .unwrap();
    path
}

#[test]
fn encodes_every_non_alphanumeric_character() {
    assert_eq!(
        encode_path("/Users/mike/my_proj.v2"),
        "-Users-mike-my-proj-v2"
    );
}

#[test]
fn resolves_symlinks_before_encoding() {
    let tmp = tmp_dir("symlinks");
    let real = tmp.join("real");
    fs::create_dir(&real).unwrap();
    let link = tmp.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let resolved = s(&fs::canonicalize(&real).unwrap());

    assert_eq!(resolve_cwd(&s(&link)), resolved);
    assert_eq!(encode_cwd(&s(&link)), encode_path(&resolved));
}

#[test]
fn keeps_a_path_that_does_not_exist() {
    let missing = s(&tmp_dir("missing").join("nope"));
    assert_eq!(resolve_cwd(&missing), missing);
}

#[test]
fn transcript_path_lives_under_the_encoded_project_dir() {
    let tmp = tmp_dir("transcript-path");
    let path = transcript_path_for("/Users/mike/proj", "abc", Some(&s(&tmp)));
    assert_eq!(
        path,
        s(&tmp
            .join(".claude")
            .join("projects")
            .join("-Users-mike-proj")
            .join("abc.jsonl"))
    );
}

#[test]
fn reads_metadata_from_a_transcript() {
    let tmp = tmp_dir("metadata");
    let path = write_session(
        &tmp,
        "/Users/mike/proj",
        "abc",
        Some("main"),
        "do the thing",
    );

    let meta = read_session_meta(&s(&path)).unwrap();

    assert_eq!(meta.session_id, "abc");
    assert_eq!(meta.cwd, "/Users/mike/proj");
    assert_eq!(meta.git_branch.as_deref(), Some("main"));
    assert_eq!(meta.version.as_deref(), Some("1.0.99"));
    assert_eq!(meta.summary.as_deref(), Some("do the thing"));
    assert!(meta.modified_at.ends_with('Z'));
}

#[test]
fn a_secret_in_the_first_message_never_reaches_the_summary() {
    let tmp = tmp_dir("secret-summary");
    let path = write_session(
        &tmp,
        "/Users/mike/proj",
        "abc",
        Some("main"),
        &format!("deploy with {}", secret()),
    );

    let meta = read_session_meta(&s(&path)).unwrap();

    assert_eq!(
        meta.summary.as_deref(),
        Some("deploy with [REDACTED:github-token]")
    );
}

/// Truncating first would leave a fragment too short for any rule to match,
/// so the tail of a secret would ride along unredacted.
#[test]
fn a_secret_is_scrubbed_before_the_summary_is_cut_to_length() {
    let tmp = tmp_dir("secret-truncate");
    let padding = "x".repeat(SUMMARY_LIMIT - 11) + " ";
    let path = write_session(
        &tmp,
        "/Users/mike/proj",
        "abc",
        Some("main"),
        &format!("{}{}", padding, secret()),
    );

    let meta = read_session_meta(&s(&path)).unwrap();
    let summary = meta.summary.unwrap_or_default();

    assert_eq!(summary.chars().count(), SUMMARY_LIMIT);
    assert!(!summary.contains("ghp_"));
}

// Skipped: test_a_summary_that_will_not_come_clean_is_dropped_whole
// monkeypatches `loch.claude.redact` to raise; Rust has no seam to force
// redact() to fail.

#[test]
fn ignores_a_transcript_with_no_cwd() {
    let tmp = tmp_dir("no-cwd");
    let path = tmp.join("empty.jsonl");
    fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
    assert!(read_session_meta(&s(&path)).is_none());
}

// A session that moved: it began in the main repo and continued in a worktree
// after the agent switched, so the records from before the move stay in the
// file and the first several hundred name a directory the session has left.
// Claude files the transcript under the cwd it ended in.
const MAIN: &str = "/Users/mike/apps/harness";
const WORKTREE: &str = "/Users/mike/apps/harness-worktrees/feat/send";

fn write_moved_session(home: &Path) -> PathBuf {
    let session_id = "abc";
    let record = |cwd: &str, branch: &str, text: &str| {
        json::dumps(
            &Value::Object(object! {
                "type" => "user",
                "sessionId" => session_id,
                "cwd" => cwd,
                "gitBranch" => branch,
                "version" => "1.0.99",
                "message" => object! {"role" => "user", "content" => text},
            }),
            json::DEFAULT,
        )
    };
    let directory = PathBuf::from(session_dir_for(WORKTREE, Some(&s(home))));
    fs::create_dir_all(&directory).unwrap();
    let mut lines: Vec<String> = (0..300)
        .map(|_| record(MAIN, "main", "started here"))
        .collect();
    lines.push(record(WORKTREE, "feat/send", "moved here"));
    let path = directory.join(format!("{}.jsonl", session_id));
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

/// The first record's cwd is stale, and the live one first appears well past
/// any fixed scan window, so the directory is the only disambiguator.
#[test]
fn prefers_the_cwd_the_project_directory_is_named_after() {
    let tmp = tmp_dir("prefers-directory");
    let path = write_moved_session(&tmp);

    let meta = read_session_meta(&s(&path)).unwrap();

    assert_eq!(meta.cwd, WORKTREE);
    assert_eq!(meta.other_cwds, vec![MAIN.to_string()]);
}

/// A branch from a directory the session has left is the same defect as the
/// stale cwd, and it decides which index entry the bundle is filed under.
#[test]
fn branch_is_read_alongside_the_chosen_cwd() {
    let tmp = tmp_dir("branch-alongside");
    let path = write_moved_session(&tmp);

    let meta = read_session_meta(&s(&path)).unwrap();

    assert_eq!(meta.git_branch.as_deref(), Some("feat/send"));
}

#[test]
fn a_moved_session_is_listed_under_the_branch_it_ended_on() {
    let tmp = tmp_dir("moved-listed");
    write_moved_session(&tmp);
    let home = s(&tmp);

    let ids: Vec<String> = list_sessions(WORKTREE, Some("feat/send"), None, Some(&home))
        .into_iter()
        .map(|meta| meta.session_id)
        .collect();
    assert_eq!(ids, vec!["abc".to_string()]);
    assert!(list_sessions(WORKTREE, Some("main"), None, Some(&home)).is_empty());
}

/// A hand-copied transcript or a renamed directory keeps the old behaviour
/// rather than dropping the session.
#[test]
fn falls_back_to_the_first_cwd_when_the_directory_matches_nothing() {
    let tmp = tmp_dir("fallback");
    let directory = tmp.join("unrelated");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("abc.jsonl");
    let lines = [
        json::dumps(
            &Value::Object(object! {"cwd" => MAIN, "gitBranch" => "main"}),
            json::DEFAULT,
        ),
        json::dumps(
            &Value::Object(object! {"cwd" => WORKTREE, "gitBranch" => "feat/send"}),
            json::DEFAULT,
        ),
    ];
    fs::write(&path, lines.join("\n")).unwrap();

    let meta = read_session_meta(&s(&path)).unwrap();

    assert_eq!(meta.cwd, MAIN);
    assert_eq!(meta.other_cwds, vec![WORKTREE.to_string()]);
}

/// `"cwd"` also occurs inside tool inputs and results — loch's own --json
/// output among them — where it names some other process's directory.
#[test]
fn transcript_cwds_counts_only_the_top_level_field() {
    let nested = json::dumps(
        &Value::Object(object! {"cwd" => "/elsewhere"}),
        json::DEFAULT,
    );
    let raw = [
        json::dumps(
            &Value::Object(object! {"cwd" => MAIN, "toolUseResult" => nested}),
            json::DEFAULT,
        ),
        json::dumps(&Value::Object(object! {"cwd" => MAIN}), json::DEFAULT),
        String::new(),
        "not json at all".to_string(),
    ]
    .join("\n");

    assert_eq!(transcript_cwds(&raw), vec![(MAIN.to_string(), 2)]);
}

#[test]
fn lists_and_filters_sessions() {
    let tmp = tmp_dir("lists");
    let home = s(&tmp);
    let cwd = "/Users/mike/proj";
    write_session(&tmp, cwd, "one", Some("main"), "hello");
    write_session(&tmp, cwd, "two", Some("feature/x"), "hello");
    let ids = |metas: Vec<loch::claude::SessionMeta>| -> Vec<String> {
        metas.into_iter().map(|meta| meta.session_id).collect()
    };

    let mut every = ids(list_sessions(cwd, None, None, Some(&home)));
    every.sort();
    assert_eq!(every, vec!["one".to_string(), "two".to_string()]);

    assert_eq!(
        ids(list_sessions(cwd, Some("feature/x"), None, Some(&home))),
        vec!["two".to_string()]
    );
    assert_eq!(
        ids(list_sessions(cwd, None, Some("one"), Some(&home))),
        vec!["one".to_string()]
    );
    assert!(list_sessions("/Users/mike/other", None, None, Some(&home)).is_empty());
}

#[test]
fn claude_config_dir_replaces_the_home_default() {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tmp_dir("config-dir");
    let saved_home = std::env::var_os("HOME");
    let saved_config = std::env::var_os("CLAUDE_CONFIG_DIR");

    std::env::set_var("HOME", tmp.join("home"));
    std::env::set_var("CLAUDE_CONFIG_DIR", tmp.join("work-account"));
    let with_config = projects_dir(None);
    // An explicit home names some other machine's layout, where this
    // machine's variable means nothing.
    let explicit = projects_dir(Some("/home/other"));
    std::env::set_var("CLAUDE_CONFIG_DIR", "");
    let empty_config = projects_dir(None);

    match saved_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match saved_config {
        Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
        None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
    }

    assert_eq!(with_config, s(&tmp.join("work-account").join("projects")));
    assert_eq!(explicit, "/home/other/.claude/projects");
    assert_eq!(
        empty_config,
        s(&tmp.join("home").join(".claude").join("projects"))
    );
}
