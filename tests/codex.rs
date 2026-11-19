//! The Codex adapter end to end, through the binary: hooks and blame, a
//! cancelled turn, `ask`'s fork, discovery's choice of cwd and branch, and
//! `save`/`restore` between two homes. Rollouts are synthetic, shaped like
//! codex-cli 0.166's: `session_meta` first, then per turn `task_started`,
//! `turn_context`, the user's message, the reply and `task_complete`.

use loch::json::{self, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const SESSION: &str = "01a1f570-8214-7ff3-8e70-1c2d9088dd3c";
const TURN_A: &str = "01a1f570-8591-7783-8fb3-db52e4b56c30";
const TURN_B: &str = "01a1f570-cafa-7000-8000-000000000002";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct World {
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    store: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn rollout_name(session: &str) -> String {
    format!("2026/11/18/rollout-2026-11-18T10-00-00-{}.jsonl", session)
}

impl World {
    fn new() -> World {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "loch-test-codex-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let world = World {
            home: root.join("home"),
            repo: root.join("repo"),
            store: root.join("store"),
            root,
        };
        fs::create_dir_all(&world.home).unwrap();
        fs::create_dir_all(&world.repo).unwrap();
        fs::write(world.root.join("gitconfig"), "").unwrap();
        world.git(&["init", "-q", "-b", "feat/x"]);
        world.git(&["commit", "-q", "--allow-empty", "-m", "initial"]);
        world
    }

    /// A second machine: its own home and checkout, the same store.
    fn elsewhere(&self, name: &str) -> World {
        let other = World {
            home: self.root.join(format!("{}-home", name)),
            repo: self.root.join(format!("{}-repo", name)),
            store: self.store.clone(),
            root: self.root.join(format!("{}-unused", name)),
        };
        fs::create_dir_all(&other.home).unwrap();
        fs::create_dir_all(&other.repo).unwrap();
        other
    }

    fn command(&self, program: &str, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("LOCH_STORE", &self.store)
            .env("GIT_CONFIG_GLOBAL", self.root_gitconfig())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("SOURCE_DATE_EPOCH");
        command
    }

    fn root_gitconfig(&self) -> PathBuf {
        let path = self.home.join(".gitconfig-empty");
        if !path.exists() {
            fs::write(&path, "").unwrap();
        }
        path
    }

    fn git(&self, args: &[&str]) {
        let out = self.command("git", &self.repo).args(args).output().unwrap();
        assert!(out.status.success(), "git {:?}", args);
    }

    fn sh(&self, script: &str) {
        let out = self
            .command("sh", &self.repo)
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(out.status.success(), "sh {}", script);
    }

    fn loch_with(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(env!("CARGO_BIN_EXE_loch"), &self.repo)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn json(&self, args: &[&str]) -> (Value, i32) {
        let mut args = args.to_vec();
        args.push("--json");
        let out = self.loch_with(&args, "");
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "one document: {}", text);
        (
            json::loads(text.trim()).unwrap(),
            out.status.code().unwrap(),
        )
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (document, code) = self.json(args);
        assert_eq!(code, 0, "{:?}", document);
        document
    }

    fn rollout(&self, session: &str) -> PathBuf {
        self.home
            .join(".codex/sessions")
            .join(rollout_name(session))
    }

    fn append(&self, session: &str, lines: &[String]) {
        let path = self.rollout(session);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
    }

    fn meta(&self, session: &str, cwd: &str, branch: &str) {
        self.append(
            session,
            &[format!(
                r#"{{"timestamp":"2026-11-18T10:00:00.000Z","type":"session_meta","payload":{{"session_id":"{s}","id":"{s}","timestamp":"2026-11-18T10:00:00.000Z","cwd":"{c}","originator":"codex_exec","cli_version":"0.166.0","git":{{"commit_hash":"abc","branch":"{b}"}}}}}}"#,
                s = session,
                c = cwd,
                b = branch
            )],
        );
    }

    /// A turn begins in the rollout: what Codex writes before the prompt hook.
    fn begin(&self, session: &str, turn: &str, cwd: &str, minute: u32, prompt: &str) {
        let at = |second: u32| format!("2026-11-18T10:{:02}:{:02}.000Z", minute, second);
        self.append(
            session,
            &[
                format!(
                    r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"thread_settings_applied","thread_id":"{}"}}}}"#,
                    at(0),
                    session
                ),
                format!(
                    r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"task_started","turn_id":"{}"}}}}"#,
                    at(1),
                    turn
                ),
                format!(
                    r#"{{"timestamp":"{}","type":"turn_context","payload":{{"turn_id":"{}","cwd":"{}"}}}}"#,
                    at(1),
                    turn,
                    cwd
                ),
                format!(
                    r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<environment_context>{}</environment_context>"}}]}}}}"#,
                    at(2),
                    cwd
                ),
                format!(
                    r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"item_completed","thread_id":"{}","turn_id":"{}","item":{{"type":"UserMessage","content":[{{"type":"text","text":"{}"}}]}}}}}}"#,
                    at(3),
                    session,
                    turn,
                    prompt
                ),
            ],
        );
    }

    fn finish(&self, session: &str, turn: &str, minute: u32) {
        self.append(
            session,
            &[
                format!(
                    r#"{{"timestamp":"2026-11-18T10:{:02}:20.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"done"}}],"internal_chat_message_metadata_passthrough":{{"turn_id":"{}"}}}}}}"#,
                    minute, turn
                ),
                format!(
                    r#"{{"timestamp":"2026-11-18T10:{:02}:21.000Z","type":"event_msg","payload":{{"type":"task_complete","turn_id":"{}"}}}}"#,
                    minute, turn
                ),
            ],
        );
    }

    fn event(&self, session: &str, turn: Option<&str>, prompt: Option<&str>) -> String {
        let mut event = format!(
            r#"{{"session_id":"{}","cwd":"{}","transcript_path":"{}""#,
            session,
            s(&self.repo),
            s(&self.rollout(session))
        );
        if let Some(turn) = turn {
            event.push_str(&format!(r#","turn_id":"{}""#, turn));
        }
        if let Some(prompt) = prompt {
            event.push_str(&format!(r#","prompt":"{}""#, prompt));
        }
        event.push('}');
        event
    }

    fn hook(&self, name: &str, event: &str) {
        let out = self.loch_with(&["hook", name, "--agent", "codex"], event);
        assert!(out.status.success() && out.stdout.is_empty());
    }

    /// One whole turn, as Codex and its hooks run it, with the shell edit
    /// `script` made in between.
    fn turn(&self, turn: &str, minute: u32, prompt: &str, script: &str) {
        let repo = s(&self.repo);
        self.begin(SESSION, turn, &repo, minute, prompt);
        self.hook("prompt", &self.event(SESSION, Some(turn), Some(prompt)));
        self.sh(script);
        self.finish(SESSION, turn, minute);
        self.hook("stop", &self.event(SESSION, Some(turn), None));
    }

    fn edit_record(&self) -> Value {
        let path = self
            .store
            .join("edits/codex")
            .join(format!("{}.json", SESSION));
        json::loads(&fs::read_to_string(path).unwrap()).unwrap()
    }
}

fn at<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("no {:?} in {:?}", key, value))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    at(value, key).as_str().unwrap_or("")
}

fn items<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    at(value, key).as_array().unwrap()
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| json::loads(line).unwrap())
        .collect()
}

// -- attribution ------------------------------------------------------------------

#[test]
fn codex_turns_are_recorded_and_blamed_with_their_agent() {
    let world = World::new();
    world.meta(SESSION, &s(&world.repo), "feat/x");
    world.turn(
        TURN_A,
        1,
        "make greet",
        "printf 'def hello():\\n    return \"hi\"\\n' > greet.py",
    );
    world.turn(
        TURN_B,
        2,
        "add bye",
        "printf '\\n\\ndef bye():\\n    return \"bye\"\\n' >> greet.py",
    );
    fs::write(
        world.home.join(".codex/session_index.jsonl"),
        format!(
            "{{\"id\":\"{0}\",\"thread_name\":\"First name\"}}\n{{\"id\":\"{0}\",\"thread_name\":\"Greeting helpers\"}}\n",
            SESSION
        ),
    )
    .unwrap();

    let record = world.edit_record();
    assert_eq!(text(&record, "agent"), "codex");
    let turns = items(&record, "turns");
    assert_eq!(turns.len(), 2);
    assert_eq!(text(&turns[0], "promptUuid"), TURN_A);
    assert_eq!(text(&turns[0], "endUuid"), TURN_A);
    assert_eq!(text(&turns[0], "prompt"), "make greet");
    assert_eq!(text(&turns[0], "started"), "2026-11-18T10:01:01.000Z");
    // The turn ends at the last record naming it, not at the next turn's
    // bookkeeping.
    assert_eq!(text(&turns[0], "ended"), "2026-11-18T10:01:21.000Z");
    assert_eq!(turns[0].get("interrupted"), Some(&Value::Bool(false)));

    let blame = world.ok(&["blame", "greet.py"]);
    let conversations = items(&blame, "conversations");
    assert_eq!(conversations.len(), 2);
    for conversation in conversations {
        assert_eq!(text(conversation, "agent"), "codex");
        assert_eq!(text(conversation, "session"), SESSION);
        assert_eq!(at(conversation, "available"), &Value::Bool(true));
        assert_eq!(text(conversation, "title"), "Greeting helpers");
    }
    let marks: Vec<(i64, i64, &str)> = items(&blame, "hunks")
        .iter()
        .map(|h| {
            (
                at(h, "start").as_i64().unwrap(),
                at(h, "end").as_i64().unwrap(),
                text(h, "mark"),
            )
        })
        .collect();
    assert_eq!(marks, vec![(1, 2, "A"), (3, 6, "B")]);

    let log = world.ok(&["log", "greet.py:5"]);
    let only = &items(&log, "conversations")[0];
    assert_eq!(text(only, "prompt"), "add bye");
}

#[test]
fn a_cancelled_codex_turn_is_closed_by_the_next_prompt() {
    let world = World::new();
    let repo = s(&world.repo);
    world.meta(SESSION, &repo, "feat/x");
    world.begin(SESSION, TURN_A, &repo, 1, "start then cancel");
    world.hook(
        "prompt",
        &world.event(SESSION, Some(TURN_A), Some("start then cancel")),
    );
    world.sh("echo 'written before the cancel' > notes.txt");
    // Codex runs no Stop for a cancelled turn; it writes turn_aborted.
    world.append(
        SESSION,
        &[format!(
            r#"{{"timestamp":"2026-11-18T10:01:30.000Z","type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"{}","reason":"interrupted"}}}}"#,
            TURN_A
        )],
    );
    // The next prompt is already in the rollout when its hook runs.
    world.begin(SESSION, TURN_B, &repo, 2, "next");
    world.hook("prompt", &world.event(SESSION, Some(TURN_B), Some("next")));
    world.finish(SESSION, TURN_B, 2);
    world.hook("stop", &world.event(SESSION, Some(TURN_B), None));

    let record = world.edit_record();
    let turns = items(&record, "turns");
    assert_eq!(turns.len(), 2);
    assert_eq!(text(&turns[0], "promptUuid"), TURN_A);
    assert_eq!(turns[0].get("interrupted"), Some(&Value::Bool(true)));
    assert_eq!(text(&turns[0], "prompt"), "start then cancel");
    assert_eq!(text(&turns[0], "ended"), "2026-11-18T10:01:30.000Z");
    assert_eq!(text(&items(&turns[0], "files")[0], "path"), "notes.txt");
    assert_eq!(text(&turns[1], "promptUuid"), TURN_B);
    assert!(items(&turns[1], "files").is_empty());
}

#[test]
fn session_end_closes_an_open_codex_turn() {
    let world = World::new();
    let repo = s(&world.repo);
    world.meta(SESSION, &repo, "feat/x");
    world.begin(SESSION, TURN_A, &repo, 1, "quit mid-turn");
    world.hook(
        "prompt",
        &world.event(SESSION, Some(TURN_A), Some("quit mid-turn")),
    );
    world.sh("echo 'left behind' > left.txt");
    // SessionEnd carries no turn id.
    world.hook("end", &world.event(SESSION, None, None));
    let turns = world.edit_record().get("turns").cloned().unwrap();
    let turn = &turns.as_array().unwrap()[0];
    assert_eq!(text(turn, "promptUuid"), TURN_A);
    assert_eq!(turn.get("interrupted"), Some(&Value::Bool(true)));
}

// -- ask --------------------------------------------------------------------------

#[test]
fn ask_forks_a_codex_session_at_the_end_of_a_turn() {
    let world = World::new();
    world.meta(SESSION, &s(&world.repo), "feat/x");
    world.turn(TURN_A, 1, "make a", "echo 'alpha line here' > a.txt");
    world.turn(TURN_B, 2, "make b", "echo 'bravo line here' > b.txt");

    let ask = world.ok(&["ask", "a.txt:1", "--at-turn", "--no-launch"]);
    let fork = at(&ask, "fork");
    let id = text(fork, "sessionId");
    assert_ne!(id, SESSION);
    let launch: Vec<&str> = items(&ask, "launch")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(launch, vec!["codex", "resume", id]);
    let path = PathBuf::from(text(fork, "path"));
    assert!(path.starts_with(world.home.join(".codex/sessions")));
    assert!(s(&path).ends_with(&format!("-{}.jsonl", id)));

    let kept = records(&path);
    assert_eq!(kept.len() as i64, at(fork, "records").as_i64().unwrap());
    let last = kept.last().unwrap();
    assert_eq!(text(at(last, "payload"), "type"), "task_complete");
    assert_eq!(text(at(last, "payload"), "turn_id"), TURN_A);
    let raw = fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(TURN_B), "the fork ends at turn A");
    assert!(
        !raw.contains(SESSION),
        "every mention of the old id is rewritten"
    );
    assert_eq!(text(at(&kept[0], "payload"), "id"), id);

    let whole = world.ok(&["ask", "a.txt:1", "--no-launch"]);
    let raw = fs::read_to_string(text(at(&whole, "fork"), "path")).unwrap();
    assert!(
        raw.contains(TURN_B),
        "without --at-turn the fork runs to the end"
    );
}

// -- discovery ----------------------------------------------------------------------

#[test]
fn list_follows_the_live_cwd_and_drops_a_branch_it_left() {
    let world = World::new();
    let repo = s(&world.repo);
    let old = s(&world.root.join("old-checkout"));
    let moved = "aaaaaaaa-0000-4000-8000-000000000001";
    let stayed = "aaaaaaaa-0000-4000-8000-000000000002";
    world.meta(moved, &old, "main");
    world.begin(moved, TURN_A, &old, 1, "in the old place");
    world.finish(moved, TURN_A, 1);
    world.begin(moved, TURN_B, &repo, 2, "after resume -C");
    world.finish(moved, TURN_B, 2);
    world.meta(stayed, &repo, "feat/x");
    world.begin(stayed, TURN_A, &repo, 1, "stayed put");

    let list = world.ok(&["list", "--agent", "codex"]);
    let sessions = items(&list, "sessions");
    assert_eq!(sessions.len(), 2);
    let find = |id: &str| {
        sessions
            .iter()
            .find(|s| text(s, "sessionId") == id)
            .unwrap()
    };
    let moved_meta = find(moved);
    assert_eq!(text(moved_meta, "agent"), "codex");
    assert_eq!(text(moved_meta, "cwd"), repo);
    // session_meta's branch was the old checkout's.
    assert_eq!(at(moved_meta, "branch"), &Value::Null);
    assert_eq!(
        items(moved_meta, "otherCwds"),
        &vec![Value::str(old.as_str())]
    );
    assert_eq!(text(moved_meta, "agentVersion"), "0.166.0");
    assert_eq!(at(moved_meta, "claudeVersion"), &Value::Null);
    assert_eq!(text(moved_meta, "summary"), "in the old place");
    assert_eq!(text(find(stayed), "branch"), "feat/x");

    // Nothing lives where the session left.
    let (elsewhere, _) = world.json(&["list", "--agent", "codex", "--cwd", &old]);
    assert!(items(&elsewhere, "sessions").is_empty());
}

#[cfg(unix)]
#[test]
fn a_cwd_recorded_through_a_symlink_is_matched_resolved() {
    let world = World::new();
    let link = world.root.join("link-to-repo");
    std::os::unix::fs::symlink(&world.repo, &link).unwrap();
    // Codex stores `-C` as given.
    world.meta(SESSION, &s(&link), "feat/x");
    world.begin(SESSION, TURN_A, &s(&link), 1, "via the link");
    let list = world.ok(&["list", "--agent", "codex"]);
    assert_eq!(items(&list, "sessions").len(), 1);
    assert_eq!(
        text(&items(&list, "sessions")[0], "branch"),
        "feat/x",
        "a link to where it began is not a move"
    );
}

#[test]
fn an_unknown_agent_is_a_usage_failure_with_a_code() {
    let world = World::new();
    let (document, code) = world.json(&["list", "--agent", "nope"]);
    assert_ne!(code, 0);
    assert_eq!(text(&document, "code"), "unknown-agent");
}

// -- transport ------------------------------------------------------------------------

#[test]
fn a_codex_session_saves_and_restores_onto_another_machine() {
    let world = World::new();
    let repo = s(&world.repo);
    let old = s(&world.root.join("old-checkout"));
    world.meta(SESSION, &old, "main");
    world.begin(SESSION, TURN_A, &old, 1, "before the move");
    world.finish(SESSION, TURN_A, 1);
    world.begin(SESSION, TURN_B, &repo, 2, &format!("now in {}", repo));
    world.finish(SESSION, TURN_B, 2);

    let save = world.ok(&["save"]);
    let saved = &items(&save, "sessions")[0];
    assert_eq!(text(saved, "agent"), "codex");
    assert_eq!(text(saved, "agentVersion"), "0.166.0");
    let reference = text(&save, "ref").to_string();

    let there = world.elsewhere("b");
    let restore = there.ok(&["restore", &reference, "--new-id"]);
    let session = &items(&restore, "sessions")[0];
    assert_eq!(text(session, "status"), "restored");
    assert_eq!(text(session, "agent"), "codex");
    let id = text(session, "sessionId");
    assert_ne!(id, SESSION);
    let path = PathBuf::from(text(session, "path"));
    // Filed under the day it began, named for its new id.
    assert_eq!(
        path,
        there
            .home
            .join(".codex/sessions/2026/11/18")
            .join(format!("rollout-2026-11-18T10-00-00-{}.jsonl", id))
    );
    assert_eq!(
        text(session, "resumeCommand"),
        format!("cd {} && codex resume {}", s(&there.repo), id)
    );
    let raw = fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(SESSION));
    assert!(!raw.contains(&repo), "the live cwd is rewritten everywhere");
    assert!(raw.contains(&format!("now in {}", s(&there.repo))));
    // The records from before the move name a directory no rewrite into one
    // target can satisfy, and are counted rather than hidden.
    let foreign = at(session, "foreignCwds").as_object().unwrap();
    assert_eq!(foreign.len(), 1);
    assert_eq!(at(&foreign[&old], "count").as_i64(), Some(2));

    // Again, with the same id: it exists now, under its file name.
    let again = there.json(&["restore", &reference]).0;
    let first = &items(&again, "sessions")[0];
    assert_eq!(text(first, "status"), "restored");
    let (skipped, code) = there.json(&["restore", &reference]);
    assert_ne!(code, 0);
    assert_eq!(text(&skipped, "code"), "nothing-restored");
    assert_eq!(text(&items(&skipped, "sessions")[0], "status"), "skipped");
}

#[test]
fn restore_finds_a_session_filed_under_another_day() {
    let world = World::new();
    world.meta(SESSION, &s(&world.repo), "feat/x");
    world.begin(SESSION, TURN_A, &s(&world.repo), 1, "hello");
    let reference = text(&world.ok(&["save"]), "ref").to_string();
    let there = world.elsewhere("b");
    let elsewhere = there
        .home
        .join(".codex/sessions/2025/01/01")
        .join(format!("rollout-2025-01-01T00-00-00-{}.jsonl", SESSION));
    fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    fs::write(&elsewhere, "{}\n").unwrap();
    let (skipped, _) = there.json(&["restore", &reference]);
    let session = &items(&skipped, "sessions")[0];
    assert_eq!(text(session, "status"), "skipped");
    assert_eq!(text(session, "path"), s(&elsewhere));
    // --force overwrites that copy rather than filing a second one.
    let forced = there.ok(&["restore", &reference, "--force"]);
    assert_eq!(text(&items(&forced, "sessions")[0], "path"), s(&elsewhere));
    assert!(!there.home.join(".codex/sessions/2026").exists());
}

#[test]
fn one_bundle_carries_both_agents_and_restores_each_to_its_own() {
    let world = World::new();
    let repo = s(&world.repo);
    world.meta(SESSION, &repo, "feat/x");
    world.begin(SESSION, TURN_A, &repo, 1, "codex here");
    let claude = "22222222-3333-4444-8555-666666666666";
    let project = world
        .home
        .join(".claude/projects")
        .join(loch::claude::encode_path(&repo));
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(format!("{}.jsonl", claude)),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{}\",\"cwd\":\"{}\",\"gitBranch\":\"feat/x\",\"version\":\"2.0.1\",\"message\":{{\"role\":\"user\",\"content\":\"claude here\"}}}}\n",
            claude, repo
        ),
    )
    .unwrap();
    // The Claude session is newer, so it comes first and names the bundle.
    world.sh(&format!(
        "touch -t 202001010000 '{}'",
        s(&world.rollout(SESSION))
    ));

    let save = world.ok(&["save"]);
    let agents: Vec<&str> = items(&save, "sessions")
        .iter()
        .map(|s| text(s, "agent"))
        .collect();
    assert_eq!(agents, vec!["claude-code", "codex"]);
    let reference = text(&save, "ref").to_string();
    let packed = fs::read(world.store.join(format!("bundles/{}.loch", reference))).unwrap();
    let bundle = loch::bundle::unpack_bundle(&packed).unwrap();
    assert_eq!(bundle.agent, "claude-code");
    assert_eq!(bundle.sessions[0].agent, None);
    assert_eq!(bundle.sessions[1].agent.as_deref(), Some("codex"));
    assert_eq!(bundle.sessions[0].claude_version.as_deref(), Some("2.0.1"));
    assert_eq!(bundle.sessions[1].claude_version, None);
    assert_eq!(bundle.sessions[1].agent_version.as_deref(), Some("0.166.0"));

    let there = world.elsewhere("b");
    let restore = there.ok(&["restore", &reference]);
    let sessions = items(&restore, "sessions");
    let claude_path = PathBuf::from(text(&sessions[0], "path"));
    assert!(claude_path.starts_with(there.home.join(".claude/projects")));
    assert!(text(&sessions[0], "resumeCommand").ends_with(&format!("claude --resume {}", claude)));
    let codex_path = PathBuf::from(text(&sessions[1], "path"));
    assert!(codex_path.starts_with(there.home.join(".codex/sessions")));
    assert!(text(&sessions[1], "resumeCommand").ends_with(&format!("codex resume {}", SESSION)));

    let remote = there.ok(&["list", "--remote", "--branch", "feat/x"]);
    let entry = &items(&remote, "entries")[0];
    assert_eq!(text(entry, "agent"), "claude-code");
    assert_eq!(at(entry, "sessions").as_i64(), Some(2));
}

#[test]
fn a_bundle_of_claude_sessions_alone_has_no_agent_fields() {
    let world = World::new();
    let repo = s(&world.repo);
    let project = world
        .home
        .join(".claude/projects")
        .join(loch::claude::encode_path(&repo));
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("33333333-3333-4444-8555-666666666666.jsonl"),
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"version\":\"2.0.1\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n",
            repo
        ),
    )
    .unwrap();
    let save = world.ok(&["save"]);
    let packed = fs::read(
        world
            .store
            .join(format!("bundles/{}.loch", text(&save, "ref"))),
    )
    .unwrap();
    let mut json = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&packed[..]), &mut json).unwrap();
    let json = String::from_utf8(json).unwrap();
    assert!(!json.contains("\"agentVersion\""));
    assert!(!json.contains("\"agent\":\"codex\""));
    assert!(json.contains("\"agent\":\"claude-code\""));
}

// -- install ----------------------------------------------------------------------------

#[test]
fn install_adds_codex_hooks_beside_the_ones_already_there() {
    let world = World::new();
    let hooks = world.repo.join(".codex/hooks.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    let theirs = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"their-status-hook","timeout":5}]}],"PreToolUse":[{"hooks":[{"type":"command","command":"their-tool-hook"}]}]}}"#;
    fs::write(&hooks, theirs).unwrap();

    let installed = world.ok(&["install", "--agent", "codex"]);
    assert_eq!(text(&installed, "command"), "install");
    assert_eq!(text(&installed, "hookCommand"), "loch");
    let codex = &items(&installed, "agents")[0];
    assert_eq!(text(codex, "agent"), "codex");
    assert_eq!(text(codex, "settings"), s(&hooks));
    assert_eq!(at(codex, "needsReview"), &Value::Bool(true));

    let written = json::loads(&fs::read_to_string(&hooks).unwrap()).unwrap();
    let groups = |event: &str| at(at(&written, "hooks"), event).as_array().unwrap().clone();
    // Theirs first and unchanged: Codex keys trust by position.
    let stop = groups("Stop");
    assert_eq!(
        json::dumps(&stop[0], json::COMPACT_UNICODE),
        r#"{"hooks":[{"type":"command","command":"their-status-hook","timeout":5}]}"#
    );
    let line = |group: &Value| text(&items(group, "hooks")[0], "command").to_string();
    assert_eq!(
        line(&stop[1]),
        "loch hook stop --agent codex 2>/dev/null || true"
    );
    assert_eq!(groups("PreToolUse").len(), 1);
    assert_eq!(
        line(&groups("UserPromptSubmit")[0]),
        "loch hook prompt --agent codex 2>/dev/null || true"
    );
    assert_eq!(
        line(&groups("SessionEnd")[0]),
        "loch hook end --agent codex 2>/dev/null || true"
    );

    let again = world.ok(&["install", "--agent", "codex"]);
    let codex = &items(&again, "agents")[0];
    assert!(items(codex, "added").is_empty());
    assert_eq!(at(codex, "needsReview"), &Value::Bool(false));
    assert_eq!(groups("Stop").len(), 2);

    // --local is machine-specific, so it goes to the user's file.
    let local = world.ok(&[
        "install",
        "--agent",
        "codex",
        "--local",
        "--command",
        "/opt/loch",
    ]);
    assert_eq!(
        text(&items(&local, "agents")[0], "settings"),
        s(&world.home.join(".codex/hooks.json"))
    );
}

#[test]
fn install_sets_up_every_agent_by_default() {
    let world = World::new();
    let installed = world.ok(&["install"]);
    let agents: Vec<&str> = items(&installed, "agents")
        .iter()
        .map(|a| text(a, "agent"))
        .collect();
    assert_eq!(agents, vec!["claude-code", "codex"]);
    // The top level stays Claude's, as it was before there were two.
    assert_eq!(
        text(&installed, "settings"),
        s(&world.repo.join(".claude/settings.json"))
    );
    assert!(world.repo.join(".codex/hooks.json").exists());
    let claude = fs::read_to_string(world.repo.join(".claude/settings.json")).unwrap();
    assert!(claude.contains("\"loch hook stop 2>/dev/null || true\""));
}
