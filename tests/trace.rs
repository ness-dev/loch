//! Commit -> conversation tracing: hooks, install, snapshots, backfill, blame,
//! log and the fork `ask` makes. Ported from test/test_trace.py.
//!
//! Hooks, blame and log run the binary as Claude Code and a user would. The
//! library calls (backfill, Resolver, fork_session) read `HOME` from the
//! process environment, so every test holds `LOCK` and sets it there.

use loch::claude::transcript_path_for;
use loch::json::{self, Map, Value};
use loch::object;
use loch::store::{FileStore, Store};
use loch::trace;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

const SESSION: &str = "11111111-2222-3333-4444-555555555555";
const CALL: &str = "toolu_commit";

static LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Env {
    tmp: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn scratch() -> PathBuf {
    let base = fs::canonicalize(std::env::temp_dir()).unwrap();
    let dir = base.join(format!(
        "loch-trace-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Everything a child process (and the library, for the calls it makes from
/// this process) needs to see: a private HOME and git config, a fixed author.
fn child_env(tmp: &Path, home: &Path) -> Vec<(String, String)> {
    let mut vars = vec![
        ("HOME".to_string(), s(home)),
        ("GIT_CONFIG_GLOBAL".to_string(), s(&tmp.join("gitconfig"))),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
    ];
    for name in ["AUTHOR", "COMMITTER"] {
        vars.push((format!("GIT_{}_NAME", name), "Test".into()));
        vars.push((format!("GIT_{}_EMAIL", name), "test@example.com".into()));
    }
    vars
}

const REMOVED: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "LOCH_STORE",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_DATE",
];

fn env() -> Env {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = scratch();
    let home = tmp.join("home");
    let repo = tmp.join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::write(tmp.join("gitconfig"), "").unwrap();
    for (key, value) in child_env(&tmp, &home) {
        std::env::set_var(key, value);
    }
    for key in REMOVED {
        std::env::remove_var(key);
    }
    let env = Env {
        tmp,
        home,
        repo,
        _guard: guard,
    };
    run(&env, &env.repo, &["init", "-q", "-b", "main"], None);
    env
}

fn run(env: &Env, repo: &Path, args: &[&str], date: Option<&str>) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    for key in REMOVED {
        command.env_remove(key);
    }
    command.envs(child_env(&env.tmp, &env.home));
    if let Some(date) = date {
        command
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit(env: &Env, name: &str, text: &str, message: &str, date: Option<&str>) -> String {
    let repo = &env.repo;
    fs::write(repo.join(name), text).unwrap();
    run(env, repo, &["add", name], None);
    run(env, repo, &["commit", "-q", "-m", message], date);
    run(env, repo, &["rev-parse", "HEAD"], None)
}

fn loch(env: &Env, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_loch"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in REMOVED {
        command.env_remove(key);
    }
    command.envs(child_env(&env.tmp, &env.home));
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn obj(value: Value) -> Map {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {:?}", other),
    }
}

fn parse(text: &str) -> Value {
    json::loads(text).unwrap_or_else(|_| panic!("not JSON: {}", text))
}

/// human prompt -> assistant Bash call -> its result, as Claude writes it.
fn records(repo: &Path, command: &str, prompt: &str) -> Vec<Map> {
    let cwd = s(repo);
    let user = object! {
        "type" => "user",
        "uuid" => "u1",
        "parentUuid" => Value::Null,
        "sessionId" => SESSION,
        "cwd" => cwd.as_str(),
        "timestamp" => "2026-01-01T00:00:00.000Z",
        "message" => object! {"role" => "user", "content" => prompt},
    };
    let assistant = object! {
        "type" => "assistant",
        "uuid" => "a1",
        "parentUuid" => "u1",
        "sessionId" => SESSION,
        "cwd" => cwd.as_str(),
        "timestamp" => "2026-01-01T00:00:10.000Z",
        "message" => object! {
            "id" => "msg1",
            "role" => "assistant",
            "content" => vec![Value::Object(object! {
                "type" => "tool_use",
                "id" => CALL,
                "name" => "Bash",
                "input" => object! {"command" => command},
            })],
        },
    };
    let result = object! {
        "type" => "user",
        "uuid" => "r1",
        "parentUuid" => "a1",
        "sessionId" => SESSION,
        "cwd" => cwd.as_str(),
        "timestamp" => "2026-01-01T00:00:20.000Z",
        "message" => object! {
            "role" => "user",
            "content" => vec![Value::Object(object! {
                "type" => "tool_result", "tool_use_id" => CALL, "content" => "ok",
            })],
        },
    };
    vec![user, assistant, result]
}

fn jsonl(records: &[Map]) -> String {
    records
        .iter()
        .map(|r| json::dumps(&Value::Object(r.clone()), json::DEFAULT) + "\n")
        .collect()
}

fn transcript(env: &Env, command: &str, prompt: &str) -> String {
    let path = transcript_path_for(&s(&env.repo), SESSION, Some(&s(&env.home)));
    fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
    fs::write(&path, jsonl(&records(&env.repo, command, prompt))).unwrap();
    path
}

fn event(repo: &Path, call: &str) -> String {
    let event = object! {
        "session_id" => SESSION,
        "cwd" => s(repo),
        "tool_name" => "Bash",
        "tool_use_id" => call,
        "tool_input" => object! {"command" => "git commit"},
    };
    json::dumps(&Value::Object(event), json::DEFAULT)
}

fn hook(env: &Env, name: &str, call: &str) {
    let output = loch(env, &env.repo, &["hook", name], &event(&env.repo, call));
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

fn noted(env: &Env) -> HashSet<String> {
    trace::noted_commits(&s(&env.repo))
}

fn read_note(env: &Env, commit: &str) -> Option<Map> {
    trace::read_note(&s(&env.repo), commit)
}

#[test]
fn hooks_note_exactly_the_commits_a_call_made() {
    let env = env();
    let old = commit(
        &env,
        "a.txt",
        "a\n",
        "old work",
        Some("2020-01-01T00:00:00Z"),
    );
    run(&env, &env.repo, &["checkout", "-q", "-b", "side"], None);
    let other = commit(
        &env,
        "b.txt",
        "b\n",
        "someone else",
        Some("2020-01-02T00:00:00Z"),
    );
    run(&env, &env.repo, &["checkout", "-q", "main"], None);

    hook(&env, "pre-tool", CALL);
    let made = commit(&env, "c.txt", "c\n", "agent work", None);
    hook(&env, "post-tool", CALL);
    assert_eq!(noted(&env), HashSet::from([made.clone()]));
    let expected = object! {
        "agent" => "claude-code",
        "call" => CALL,
        "captured" => "hook",
        "loch" => 1i64,
        "session" => SESSION,
    };
    // IndexMap equality ignores order, as dict equality does.
    assert_eq!(read_note(&env, &made).unwrap(), expected);

    // Moving HEAD onto commits someone else made is not making them.
    hook(&env, "pre-tool", "toolu_checkout");
    run(&env, &env.repo, &["checkout", "-q", "side"], None);
    hook(&env, "post-tool", "toolu_checkout");
    assert_eq!(noted(&env), HashSet::from([made]));
    assert!(read_note(&env, &other).is_none());
    assert!(read_note(&env, &old).is_none());
}

#[test]
fn a_call_that_leaves_head_alone_notes_nothing() {
    let env = env();
    commit(&env, "a.txt", "a\n", "first", None);
    hook(&env, "pre-tool", CALL);
    hook(&env, "post-tool", CALL);
    assert!(noted(&env).is_empty());
    // The post hook consumed the pending state either way.
    let pending = env.repo.join(".git/loch/pending");
    assert_eq!(fs::read_dir(pending).unwrap().count(), 0);
}

#[test]
fn hook_command_never_fails() {
    let env = env();
    let output = loch(&env, &env.repo, &["hook", "post-tool"], "not json");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

#[test]
fn install_is_idempotent_and_keeps_existing_settings() {
    let env = env();
    let settings = env.tmp.join(".claude").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        r#"{"model": "x", "hooks": {"Stop": [{"hooks": []}]}}"#,
    )
    .unwrap();
    let path = s(&settings);
    assert_eq!(
        trace::install_hooks(&path, "loch").unwrap(),
        vec!["PreToolUse", "PostToolUse", "Stop"]
    );
    assert!(trace::install_hooks(&path, "loch").unwrap().is_empty());
    let written = parse(&fs::read_to_string(&settings).unwrap());
    assert!(written.get("model").unwrap().is_str("x"));
    let hooks = written.get("hooks").unwrap();
    let pre = hooks.get("PreToolUse").unwrap().as_array().unwrap();
    assert!(pre[0].get("matcher").unwrap().is_str("Bash"));
    assert_eq!(hooks.get("Stop").unwrap().as_array().unwrap().len(), 2);
}

#[test]
fn stop_snapshots_the_transcript() {
    let env = env();
    let path = transcript(&env, "git commit -m 'x'", "please add the file");
    let store = FileStore::new(s(&env.tmp.join("store")));
    let event = object! {"session_id" => SESSION, "transcript_path" => path.as_str()};
    let Ok(Some(key)) = loch::hook::snapshot_transcript(&event, &store) else {
        panic!("no snapshot taken");
    };
    assert_eq!(store.get(&key).unwrap(), fs::read(&path).unwrap());
}

#[test]
fn stop_hook_snapshots_through_the_binary() {
    let env = env();
    let path = transcript(&env, "git commit -m 'x'", "please add the file");
    let stdin = json::dumps(
        &Value::Object(object! {"session_id" => SESSION, "transcript_path" => path.as_str()}),
        json::DEFAULT,
    );
    let output = loch(&env, &env.repo, &["hook", "stop"], &stdin);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    // With no LOCH_STORE the snapshot lands in the default store under HOME.
    let store = FileStore::new(s(&env.home.join(".loch").join("store")));
    let keys = store.list(&trace::transcripts_prefix()).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(store.get(&keys[0]).unwrap(), fs::read(&path).unwrap());
}

#[test]
fn a_deleted_transcript_is_found_in_the_store() {
    let env = env();
    let path = transcript(&env, "git commit -m 'add a'", "please add the file");
    hook(&env, "pre-tool", CALL);
    let sha = commit(&env, "a.txt", "a\n", "add a", None);
    hook(&env, "post-tool", CALL);
    let store = FileStore::new(s(&env.tmp.join("store")));
    let event = object! {"session_id" => SESSION, "transcript_path" => path.as_str()};
    assert!(matches!(
        loch::hook::snapshot_transcript(&event, &store),
        Ok(Some(_))
    ));
    fs::remove_file(&path).unwrap();

    let link = trace::Resolver::new(&s(&env.repo), Some(&store))
        .resolve(&sha)
        .unwrap()
        .expect("the commit resolves");
    assert!(link.transcript.is_some());
    assert_eq!(link.prompt.as_deref(), Some("please add the file"));
}

#[test]
fn backfill_links_only_an_exact_match() {
    let env = env();
    // The transcript's call ran 00:00:10-00:00:20; these straddle it.
    let inside = commit(&env, "a.txt", "a\n", "add a", Some("2026-01-01T00:00:15Z"));
    let outside = commit(&env, "b.txt", "b\n", "add a", Some("2026-01-01T05:00:00Z"));
    let unnamed = commit(&env, "c.txt", "c\n", "add c", Some("2026-01-01T00:00:15Z"));
    transcript(
        &env,
        r#"git add a.txt && git commit -m "add a""#,
        "please add the file",
    );

    let repo = s(&env.repo);
    let mut seen: Vec<(usize, usize, u64, u64)> = Vec::new();
    let mut progress = |a: usize, b: usize, c: u64, d: u64| seen.push((a, b, c, d));
    let linked = trace::backfill(&repo, "HEAD", Some(&mut progress)).unwrap();
    assert_eq!(
        linked
            .iter()
            .map(|(sha, _)| sha.clone())
            .collect::<Vec<_>>(),
        vec![inside.clone()]
    );
    // One step per transcript, ending with everything read.
    let last = *seen.last().expect("progress was reported");
    assert!(last.0 == last.1 && last.2 == last.3 && last.3 > 0);
    let note = read_note(&env, &inside).unwrap();
    assert!(note.get("captured").unwrap().is_str("backfill"));
    assert!(read_note(&env, &outside).is_none());
    assert!(read_note(&env, &unnamed).is_none());
    assert!(trace::backfill(&repo, "HEAD", None).unwrap().is_empty());
}

#[test]
fn blame_names_the_prompt_behind_each_line() {
    let env = env();
    commit(&env, "f.txt", "human line\n", "by hand", None);
    transcript(&env, "git commit -am 'agent'", "make it two lines");
    hook(&env, "pre-tool", CALL);
    let sha = commit(&env, "f.txt", "human line\nagent line\n", "agent", None);
    hook(&env, "post-tool", CALL);

    let output = loch(&env, &env.repo, &["blame", "f.txt", "--json"], "");
    assert!(output.status.success());
    let result = parse(&String::from_utf8(output.stdout).unwrap());
    let hunks = result.get("hunks").unwrap().as_array().unwrap();
    let shape: Vec<(i64, bool)> = hunks
        .iter()
        .map(|h| {
            (
                h.get("start").unwrap().as_i64().unwrap(),
                matches!(h.get("conversation"), Some(Value::Null)),
            )
        })
        .collect();
    assert_eq!(shape, vec![(1, true), (2, false)]);
    assert!(hunks[1].get("commit").unwrap().is_str(&sha));
    let conversation = hunks[1].get("conversation").unwrap();
    assert!(conversation
        .get("prompt")
        .unwrap()
        .is_str("make it two lines"));
    assert!(conversation
        .get("turn")
        .unwrap()
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

#[test]
fn log_finds_conversations_blame_has_painted_over() {
    let env = env();
    let first = commit(&env, "f.txt", "one\nother\n", "by hand", None);
    transcript(&env, "git commit -am 'agent'", "reword line one");
    hook(&env, "pre-tool", CALL);
    let agent = commit(&env, "f.txt", "uno\nother\n", "agent", None);
    hook(&env, "post-tool", CALL);
    let last = commit(&env, "f.txt", "ONE\nother\n", "by hand again", None);
    commit(&env, "f.txt", "ONE\nelse\n", "only line two", None);

    let log = |target: &str| -> Value {
        let output = loch(&env, &env.repo, &["log", target, "--json"], "");
        assert!(output.status.success());
        parse(&String::from_utf8(output.stdout).unwrap())
    };

    // blame of line 1 sees only `last`; its history still holds the agent.
    let line = log("f.txt:1");
    let commits = line.get("commits").unwrap().as_array().unwrap();
    let shas: Vec<&str> = commits
        .iter()
        .map(|c| c.get("commit").unwrap().as_str().unwrap())
        .collect();
    assert_eq!(shas, vec![last.as_str(), agent.as_str(), first.as_str()]);
    let unlinked: Vec<bool> = commits
        .iter()
        .map(|c| matches!(c.get("conversation"), Some(Value::Null)))
        .collect();
    assert_eq!(unlinked, vec![true, false, true]);
    let lines = line.get("lines").unwrap();
    assert_eq!(obj(lines.clone()).len(), 2);
    assert_eq!(lines.get("start").unwrap().as_i64(), Some(1));
    assert_eq!(lines.get("end").unwrap().as_i64(), Some(1));
    let conversations = line.get("conversations").unwrap().as_array().unwrap();
    let prompts: Vec<&str> = conversations
        .iter()
        .map(|c| c.get("prompt").unwrap().as_str().unwrap())
        .collect();
    assert_eq!(prompts, vec!["reword line one"]);
    assert_eq!(
        conversations[0].get("commits").unwrap(),
        &Value::Array(vec![Value::str(agent.as_str())])
    );
    assert_eq!(
        log("f.txt")
            .get("commits")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        4
    );

    let output = loch(&env, &env.repo, &["log", "f.txt:9", "--json"], "");
    assert!(!output.status.success());
    let failure = parse(&String::from_utf8(output.stdout).unwrap());
    assert!(failure.get("code").unwrap().is_str("usage"));
}

fn set_path(record: &mut Map, path: &[&str], index: Option<usize>, key: &str, value: &str) {
    let mut node = record;
    for part in path {
        node = match node.get_mut(*part) {
            Some(Value::Object(next)) => next,
            _ => panic!("no object at {}", part),
        };
    }
    let node = match index {
        Some(i) => match node.get_mut("content") {
            Some(Value::Array(items)) => match &mut items[i] {
                Value::Object(item) => item,
                _ => panic!("not an object"),
            },
            _ => panic!("no content"),
        },
        None => node,
    };
    node.insert(key.into(), Value::str(value));
}

fn read_jsonl(path: &str) -> Vec<Map> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| obj(parse(line)))
        .collect()
}

fn with(record: &Map, fields: &[(&str, Value)]) -> Map {
    let mut copy = record.clone();
    for (key, value) in fields {
        copy.insert(key.to_string(), value.clone());
    }
    copy
}

fn uuids(records: &[Map]) -> Vec<Option<&str>> {
    records
        .iter()
        .map(|r| r.get("uuid").and_then(Value::as_str))
        .collect()
}

#[test]
fn fork_cuts_after_the_whole_round_and_leaves_the_original() {
    let env = env();
    let path = transcript(&env, "git commit", "please add the file");
    let records = read_jsonl(&path);
    // A parallel second call in the same message, with its result after ours,
    // then the conversation carrying on past the commit.
    let mut second = with(
        &records[1],
        &[("uuid", "a2".into()), ("parentUuid", "a1".into())],
    );
    set_path(&mut second, &["message"], Some(0), "id", "toolu_other");
    let result_one = with(&records[2], &[("parentUuid", "a2".into())]);
    let mut result_two = with(
        &records[2],
        &[("uuid", "r2".into()), ("parentUuid", "r1".into())],
    );
    set_path(
        &mut result_two,
        &["message"],
        Some(0),
        "tool_use_id",
        "toolu_other",
    );
    let later = with(
        &records[0],
        &[
            ("uuid", "u2".into()),
            ("parentUuid", "r2".into()),
            ("message", Value::Object(object! {"content" => "more"})),
        ],
    );
    let lines = vec![
        records[0].clone(),
        records[1].clone(),
        second,
        result_one,
        result_two,
        later,
    ];
    let original = jsonl(&lines);
    fs::write(&path, &original).unwrap();

    let target = env.tmp.join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    let Ok(fork) = trace::fork_session(&path, &s(&target), Some(CALL), None) else {
        panic!("fork failed");
    };

    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    let forked = read_jsonl(&fork.path);
    assert_eq!(
        uuids(&forked),
        vec![
            Some("u1"),
            Some("a1"),
            Some("a2"),
            Some("r1"),
            Some("r2"),
            None
        ]
    );
    let sessions: HashSet<&str> = forked
        .iter()
        .map(|r| r.get("sessionId").unwrap().as_str().unwrap())
        .collect();
    assert_eq!(sessions, HashSet::from([fork.session_id.as_str()]));
    let cwds: HashSet<&str> = forked
        .iter()
        .filter_map(|r| r.get("cwd"))
        .map(|c| c.as_str().unwrap())
        .collect();
    let target_text = s(&target);
    assert_eq!(cwds, HashSet::from([target_text.as_str()]));
    assert!(forked
        .last()
        .unwrap()
        .get("type")
        .unwrap()
        .is_str("custom-title"));
    assert_eq!(
        fork.path,
        transcript_path_for(&target_text, &fork.session_id, None)
    );
}

#[test]
fn fork_without_a_call_runs_to_the_end_of_the_conversation() {
    let env = env();
    let path = transcript(&env, "git commit", "please add the file");
    let mut records = read_jsonl(&path);
    let later = with(
        &records[0],
        &[
            ("uuid", "u2".into()),
            ("parentUuid", "r1".into()),
            ("message", Value::Object(object! {"content" => "more"})),
        ],
    );
    records.push(later);
    fs::write(&path, jsonl(&records)).unwrap();

    let Ok(fork) = trace::fork_session(&path, &s(&env.repo), None, None) else {
        panic!("fork failed");
    };
    let forked = read_jsonl(&fork.path);
    assert_eq!(
        uuids(&forked),
        vec![Some("u1"), Some("a1"), Some("r1"), Some("u2"), None]
    );
}

#[test]
fn blame_html_marks_every_line_with_its_conversation() {
    let env = env();
    commit(&env, "f.txt", "human line\n", "by hand", None);
    transcript(&env, "git commit -am 'agent'", "make it <two> lines");
    hook(&env, "pre-tool", CALL);
    commit(&env, "f.txt", "human line\nagent line\n", "agent", None);
    hook(&env, "post-tool", CALL);
    fs::write(
        env.repo.join("f.txt"),
        "human line\nagent line\nwork in progress\n",
    )
    .unwrap();

    let page = env.tmp.join("blame.html");
    let output = loch(
        &env,
        &env.repo,
        &["blame", "f.txt", "--html", &s(&page), "--no-open"],
        "",
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("wrote {}\n", s(&page))
    );
    let html = fs::read_to_string(&page).unwrap();
    assert!(html.contains(r#"<tr data-mark="·" data-line="1""#));
    assert!(html.contains(r#"<tr data-mark="A" data-line="2""#));
    assert!(html.contains(r#"<tr data-mark="+" data-line="3""#));
    assert!(html.contains("make it &lt;two&gt; lines"));
    assert!(!html.split("<script>").next().unwrap().contains("<two>"));
}
