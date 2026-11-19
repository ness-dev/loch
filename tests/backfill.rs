//! `loch backfill`: attribution for turns from before the hooks, rebuilt from
//! the transcripts' edit calls and the commits the agent made; and blame
//! following a file through renames.
//!
//! Runs the binary in a scratch HOME and store, so nothing touches this
//! machine's `~/.loch` or `~/.claude`.

use loch::json::{self, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

const ONE: &str = "11111111-2222-3333-4444-555555555555";
const TWO: &str = "22222222-3333-4444-5555-666666666666";

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

impl World {
    fn new() -> World {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "loch-test-backfill-{}-{}",
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
        world.git(&["init", "-q", "-b", "main"]);
        world.git_at(
            "2026-01-01T00:00:00Z",
            &["commit", "-q", "--allow-empty", "-m", "initial"],
        );
        world
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("LOCH_STORE", &self.store)
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GIT_DIR");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git_at(&self, date: &str, args: &[&str]) -> String {
        let out = self
            .command("git")
            .args(args)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.git(&["rev-parse", "HEAD"])
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn loch(&self, args: &[&str], stdin: &str) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        let mut child = self
            .command(env!("CARGO_BIN_EXE_loch"))
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
    }

    /// A transcript as Claude Code files it, from (record) lines.
    fn transcript(&self, session: &str, records: &[String]) -> PathBuf {
        let encoded: String = s(&self.repo)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = self.home.join(".claude/projects").join(encoded);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.jsonl", session));
        fs::write(&path, records.join("\n") + "\n").unwrap();
        path
    }

    fn prompt(&self, session: &str, uuid: &str, at: &str, text: &str) -> String {
        format!(
            r#"{{"type": "user", "uuid": "{}", "sessionId": "{}", "cwd": "{}", "timestamp": "{}", "message": {{"role": "user", "content": "{}"}}}}"#,
            uuid,
            session,
            s(&self.repo),
            at,
            text
        )
    }

    fn tool(
        &self,
        session: &str,
        uuid: &str,
        at: &str,
        id: &str,
        name: &str,
        input: &str,
    ) -> String {
        format!(
            r#"{{"type": "assistant", "uuid": "{}", "sessionId": "{}", "cwd": "{}", "timestamp": "{}", "message": {{"role": "assistant", "content": [{{"type": "tool_use", "id": "{}", "name": "{}", "input": {}}}]}}}}"#,
            uuid,
            session,
            s(&self.repo),
            at,
            id,
            name,
            input
        )
    }

    fn result(&self, session: &str, uuid: &str, at: &str, id: &str, error: bool) -> String {
        format!(
            r#"{{"type": "user", "uuid": "{}", "sessionId": "{}", "cwd": "{}", "timestamp": "{}", "message": {{"role": "user", "content": [{{"type": "tool_result", "tool_use_id": "{}", "content": "ok", "is_error": {}}}]}}}}"#,
            uuid,
            session,
            s(&self.repo),
            at,
            id,
            error
        )
    }

    fn file(&self, name: &str) -> String {
        s(&self.repo.join(name))
    }
}

fn owners(blame: &Value) -> Vec<Option<String>> {
    let mut lines = Vec::new();
    for hunk in blame.get("hunks").unwrap().as_array().unwrap() {
        let start = hunk.get("start").unwrap().as_i64().unwrap();
        let end = hunk.get("end").unwrap().as_i64().unwrap();
        let prompt = hunk
            .get("conversation")
            .and_then(|c| c.get("prompt"))
            .and_then(Value::as_str)
            .map(str::to_string);
        for _ in start..=end {
            lines.push(prompt.clone());
        }
    }
    lines
}

fn some(text: &str) -> Option<String> {
    Some(text.to_string())
}

#[test]
fn edit_tool_lines_are_recovered_and_a_failed_edit_contributes_nothing() {
    let w = World::new();
    w.write(
        "app.py",
        "def one():\n    return 1\n\ndef two():\n    return 2\n",
    );
    let write = format!(
        r#"{{"file_path": "{}", "content": "def one():\n    return 1\n\ndef two():\n    return 2\n"}}"#,
        w.file("app.py")
    );
    let failed = format!(
        r#"{{"file_path": "{}", "old_string": "x", "new_string": "def two():"}}"#,
        w.file("app.py")
    );
    w.transcript(
        ONE,
        &[
            w.prompt(ONE, "p1", "2026-02-01T00:00:00Z", "write the app"),
            w.tool(ONE, "a1", "2026-02-01T00:00:01Z", "t1", "Write", &write),
            w.result(ONE, "r1", "2026-02-01T00:00:02Z", "t1", false),
            w.prompt(ONE, "p2", "2026-02-01T00:01:00Z", "a failed edit"),
            w.tool(ONE, "a2", "2026-02-01T00:01:01Z", "t2", "Edit", &failed),
            w.result(ONE, "r2", "2026-02-01T00:01:02Z", "t2", true),
        ],
    );
    let result = w.loch(&["backfill"], "");
    assert_eq!(result.get("turns").unwrap().as_i64(), Some(1));
    let lines = owners(&w.loch(&["blame", "app.py"], ""));
    assert_eq!(lines[0], some("write the app"));
    assert_eq!(
        lines[3],
        some("write the app"),
        "the failed edit wrote nothing"
    );
    let blame = w.loch(&["blame", "app.py"], "");
    let conversation = &blame.get("conversations").unwrap().as_array().unwrap()[0];
    assert!(conversation.get("backfilled").unwrap().truthy());
}

#[test]
fn a_shell_edit_is_recovered_through_the_commit_that_made_it() {
    let w = World::new();
    w.write("cfg.txt", "mode = fast\n");
    w.git(&["add", "cfg.txt"]);
    w.git_at(
        "2026-03-01T00:00:00Z",
        &["commit", "-q", "-m", "add config"],
    );
    // The agent changed it with sed and committed: nothing in the transcript
    // shows the new text, but the commit's diff does.
    w.write("cfg.txt", "mode = safe\n");
    w.git(&["add", "cfg.txt"]);
    w.git_at(
        "2026-03-02T00:00:05Z",
        &["commit", "-q", "-m", "make it safe"],
    );
    let bash = r#"{"command": "sed -i s/fast/safe/ cfg.txt && git commit -am 'make it safe'"}"#;
    w.transcript(
        ONE,
        &[
            w.prompt(ONE, "p1", "2026-03-02T00:00:00Z", "be safer"),
            w.tool(ONE, "a1", "2026-03-02T00:00:03Z", "b1", "Bash", bash),
            w.result(ONE, "r1", "2026-03-02T00:00:07Z", "b1", false),
        ],
    );
    let result = w.loch(&["backfill"], "");
    let session = &result.get("sessions").unwrap().as_array().unwrap()[0];
    assert_eq!(session.get("commits").unwrap().as_i64(), Some(1));
    assert_eq!(
        owners(&w.loch(&["blame", "cfg.txt"], "")),
        vec![some("be safer")]
    );
}

#[test]
fn a_commit_two_sessions_could_have_made_stays_unlinked() {
    let w = World::new();
    w.write("x.txt", "contested line here\n");
    w.git(&["add", "x.txt"]);
    w.git_at("2026-04-01T00:00:05Z", &["commit", "-q", "-m", "contested"]);
    let bash = r#"{"command": "git commit -m contested"}"#;
    for session in [ONE, TWO] {
        w.transcript(
            session,
            &[
                w.prompt(session, "p1", "2026-04-01T00:00:00Z", "either of us"),
                w.tool(session, "a1", "2026-04-01T00:00:03Z", "b1", "Bash", bash),
                w.result(session, "r1", "2026-04-01T00:00:07Z", "b1", false),
            ],
        );
    }
    w.loch(&["backfill"], "");
    assert_eq!(owners(&w.loch(&["blame", "x.txt"], "")), vec![None]);
}

#[test]
fn turns_the_hooks_recorded_are_kept_and_a_rerun_changes_nothing() {
    let w = World::new();
    let transcript = w.transcript(
        ONE,
        &[w.prompt(ONE, "p1", "2026-05-01T00:00:00Z", "live turn")],
    );
    let event = format!(
        r#"{{"session_id": "{}", "cwd": "{}", "transcript_path": "{}", "prompt": "live turn"}}"#,
        ONE,
        s(&w.repo),
        s(&transcript)
    );
    w.loch(&["hook", "prompt"], &event);
    w.write("live.py", "recorded_live = True\n");
    w.loch(&["hook", "stop"], &event);
    // An earlier turn the hooks never saw, which only the transcript knows.
    let write = format!(
        r#"{{"file_path": "{}", "content": "from_the_transcript = 1\n"}}"#,
        w.file("old.py")
    );
    w.write("old.py", "from_the_transcript = 1\n");
    let mut records = vec![w.prompt(ONE, "p1", "2026-05-01T00:00:00Z", "live turn")];
    records.push(w.prompt(ONE, "p0", "2026-04-30T00:00:00Z", "missed turn"));
    records.push(w.tool(ONE, "a0", "2026-04-30T00:00:01Z", "t0", "Write", &write));
    records.push(w.result(ONE, "r0", "2026-04-30T00:00:02Z", "t0", false));
    fs::write(&transcript, records.join("\n") + "\n").unwrap();

    w.loch(&["backfill"], "");
    let record_path = w
        .home
        .join(".loch/edits/claude-code")
        .join(format!("{}.json", ONE));
    let first = fs::read_to_string(&record_path).unwrap();
    w.loch(&["backfill"], "");
    assert_eq!(
        fs::read_to_string(&record_path).unwrap(),
        first,
        "re-running is idempotent"
    );
    assert_eq!(
        owners(&w.loch(&["blame", "live.py"], "")),
        vec![some("live turn")]
    );
    assert_eq!(
        owners(&w.loch(&["blame", "old.py"], "")),
        vec![some("missed turn")]
    );
    let turns = json::loads(&first).unwrap();
    let flags: Vec<bool> = turns
        .get("turns")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.get("backfilled").unwrap().truthy())
        .collect();
    assert_eq!(
        flags,
        vec![true, false],
        "sorted by when they ended; the hook's turn kept as recorded"
    );
}

#[test]
fn the_old_design_s_git_notes_are_imported() {
    let w = World::new();
    w.write("noted.txt", "made by a noted call\n");
    w.git(&["add", "noted.txt"]);
    // The subject isn't in the command, so only the note can link it.
    let sha = w.git_at(
        "2026-06-01T09:00:00Z",
        &["commit", "-q", "-m", "something unrelated"],
    );
    let note = format!(
        r#"{{"loch":1,"agent":"claude-code","session":"{}","call":"b1","captured":"hook"}}"#,
        ONE
    );
    w.git(&[
        "notes",
        "--ref",
        "refs/notes/loch",
        "add",
        "-m",
        &note,
        &sha,
    ]);
    let bash = r#"{"command": "./scripts/commit-everything.sh"}"#;
    w.transcript(
        ONE,
        &[
            w.prompt(ONE, "p1", "2026-06-01T00:00:00Z", "run the script"),
            w.tool(ONE, "a1", "2026-06-01T00:00:01Z", "b1", "Bash", bash),
            w.result(ONE, "r1", "2026-06-01T00:00:02Z", "b1", false),
        ],
    );
    w.loch(&["backfill"], "");
    assert_eq!(
        owners(&w.loch(&["blame", "noted.txt"], "")),
        vec![some("run the script")]
    );
}

#[test]
fn blame_follows_a_file_through_a_rename() {
    let w = World::new();
    let transcript = w.transcript(
        ONE,
        &[w.prompt(ONE, "p1", "2026-07-01T00:00:00Z", "write it")],
    );
    let event = format!(
        r#"{{"session_id": "{}", "cwd": "{}", "transcript_path": "{}", "prompt": "write it"}}"#,
        ONE,
        s(&w.repo),
        s(&transcript)
    );
    w.loch(&["hook", "prompt"], &event);
    w.write("old/name.rs", "fn renamed_later() {}\n");
    w.loch(&["hook", "stop"], &event);
    w.git(&["add", "-A"]);
    w.git(&["commit", "-q", "-m", "add"]);
    w.git(&["mv", "old/name.rs", "new_name.rs"]);
    w.git(&["commit", "-q", "-m", "rename"]);
    assert_eq!(
        owners(&w.loch(&["blame", "new_name.rs"], "")),
        vec![some("write it")]
    );
}
