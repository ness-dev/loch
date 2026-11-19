//! Turns, recorded by the hooks and found again by content: `install`, the
//! `prompt`/`stop` hooks, and `blame`, `log` and `ask` on top of them.
//!
//! Everything runs through the binary, as the harness would run it, in a
//! scratch HOME with its own store, so no test touches this machine's
//! `~/.loch` or `~/.claude`.

use loch::json::{self, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const SESSION: &str = "11111111-2222-3333-4444-555555555555";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct World {
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    store: PathBuf,
    transcript: PathBuf,
    minute: usize,
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
    fn new(git: bool) -> World {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "loch-test-trace-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let repo = root.join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&repo).unwrap();
        fs::write(root.join("gitconfig"), "").unwrap();
        let world = World {
            transcript: home
                .join(".claude/projects/-repo")
                .join(format!("{}.jsonl", SESSION)),
            store: root.join("store"),
            root,
            home,
            repo,
            minute: 0,
        };
        fs::create_dir_all(world.transcript.parent().unwrap()).unwrap();
        if git {
            world.git(&world.repo, &["init", "-q", "-b", "main"]);
            world.git(
                &world.repo,
                &["commit", "-q", "--allow-empty", "-m", "initial"],
            );
        }
        world
    }

    fn command(&self, program: &str, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("LOCH_STORE", &self.store)
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("SOURCE_DATE_EPOCH");
        command
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.command("git", cwd).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn loch_in(&self, cwd: &Path, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(env!("CARGO_BIN_EXE_loch"), cwd)
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

    fn loch(&self, args: &[&str]) -> Output {
        self.loch_in(&self.repo, args, "")
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        let out = self.loch(&args);
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "one document: {}", text);
        json::loads(text.trim()).unwrap()
    }

    fn record(&self, line: String) {
        let mut existing = fs::read_to_string(&self.transcript).unwrap_or_default();
        existing.push_str(&line);
        existing.push('\n');
        fs::write(&self.transcript, existing).unwrap();
    }

    fn event(&self) -> String {
        format!(
            r#"{{"session_id": "{}", "cwd": "{}", "transcript_path": "{}"}}"#,
            SESSION,
            s(&self.repo),
            s(&self.transcript)
        )
    }

    /// A prompt is submitted: its record is written and the hook runs.
    fn prompt(&mut self, uuid: &str, text: &str) {
        self.minute += 1;
        self.record(format!(
            r#"{{"type": "user", "uuid": "{}", "parentUuid": null, "sessionId": "{}", "cwd": "{}", "timestamp": "2026-11-18T10:{:02}:00.000Z", "message": {{"role": "user", "content": "{}"}}}}"#,
            uuid,
            SESSION,
            s(&self.repo),
            self.minute,
            text
        ));
        // As Claude Code sends it: the prompt's text rides on the event.
        let event = format!(
            r#"{{"session_id": "{}", "cwd": "{}", "transcript_path": "{}", "prompt": "{}"}}"#,
            SESSION,
            s(&self.repo),
            s(&self.transcript),
            text
        );
        let out = self.loch_in(&self.repo, &["hook", "prompt"], &event);
        assert!(out.status.success() && out.stdout.is_empty());
    }

    fn hook(&self, name: &str) {
        let out = self.loch_in(&self.repo, &["hook", name], &self.event());
        assert!(out.status.success() && out.stdout.is_empty());
    }

    fn session_state(&self) -> PathBuf {
        self.home
            .join(".loch/sessions")
            .join(format!("{}.json", SESSION))
    }

    /// Everything under ~/.loch made to look long untouched: objects past
    /// collection's grace period, and session state past ABANDONED.
    fn age_everything(&self) {
        self.age(".loch");
    }

    fn age(&self, under: &str) {
        self.sh(&format!(
            "find '{}' -exec touch -t 202001010000 {{}} +",
            s(&self.home.join(under))
        ));
    }

    fn objects(&self) -> usize {
        fs::read_dir(self.home.join(".loch/objects"))
            .map(|dirs| {
                dirs.flatten()
                    .map(|d| fs::read_dir(d.path()).map(|f| f.count()).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// The agent stops: its reply is written and the hook runs.
    fn stop(&mut self, prompt_uuid: &str) {
        self.record(format!(
            r#"{{"type": "assistant", "uuid": "{}-end", "parentUuid": "{}", "sessionId": "{}", "cwd": "{}", "timestamp": "2026-11-18T10:{:02}:30.000Z", "message": {{"role": "assistant", "content": [{{"type": "text", "text": "done"}}]}}}}"#,
            prompt_uuid,
            prompt_uuid,
            SESSION,
            s(&self.repo),
            self.minute
        ));
        let out = self.loch_in(&self.repo, &["hook", "stop"], &self.event());
        assert!(out.status.success() && out.stdout.is_empty());
    }

    /// What the agent's shell does: write files however it likes.
    fn sh(&self, script: &str) {
        let out = self
            .command("sh", &self.repo)
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

/// Each line's owner, as prompt text or None.
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

fn calculator_world() -> World {
    let mut w = World::new(true);
    w.write("notes.txt", "written by a person before loch\n");
    w.prompt("p1", "write the calculator");
    w.sh("cat > calc.py <<'EOF'\ndef add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\nEOF");
    w.stop("p1");
    w.prompt("p2", "make sub safer and generate the table");
    w.sh("sed -i.bak 's/return a - b/return max(a - b, 0)/' calc.py && rm calc.py.bak");
    w.sh("for i in 0 1 2; do echo \"row_$i = $((i * i))\"; done > table.py");
    w.stop("p2");
    w
}

#[test]
fn install_writes_the_two_turn_hooks_and_keeps_existing_settings() {
    let w = World::new(false);
    let settings = w.repo.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        r#"{"model": "x", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other"}]}]}}"#,
    )
    .unwrap();
    // No repository is needed: the hooks record files, not commits.
    let first = w.json(&["install"]);
    assert_eq!(
        first.get("added").unwrap(),
        &json::loads(r#"["UserPromptSubmit", "Stop", "SessionEnd"]"#).unwrap()
    );
    let again = w.json(&["install"]);
    assert_eq!(again.get("added").unwrap(), &Value::Array(vec![]));
    let written = json::loads(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(written.get("model").unwrap().is_str("x"));
    let stop = written
        .get("hooks")
        .unwrap()
        .get("Stop")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(
        stop.len(),
        2,
        "the existing Stop hook is kept beside loch's"
    );
}

#[test]
fn install_retires_the_per_command_hooks_it_used_to_write() {
    let w = World::new(true);
    let settings = w.repo.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        r#"{"hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "loch hook pre-tool 2>/dev/null || true"}]}],
            "PostToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "loch hook post-tool 2>/dev/null || true"},
                {"type": "command", "command": "keep-me"}]}]}}"#,
    )
    .unwrap();
    let result = w.json(&["install"]);
    assert_eq!(
        result.get("removed").unwrap(),
        &json::loads(r#"["PreToolUse", "PostToolUse"]"#).unwrap()
    );
    let hooks = json::loads(&fs::read_to_string(&settings).unwrap()).unwrap();
    let hooks = hooks.get("hooks").unwrap();
    assert!(
        hooks.get("PreToolUse").is_none(),
        "an emptied event is dropped"
    );
    let post = hooks.get("PostToolUse").unwrap().as_array().unwrap();
    assert_eq!(post[0].get("hooks").unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn edits_made_by_shell_commands_are_attributed_to_their_turn() {
    let w = calculator_world();
    let blame = w.json(&["blame", "calc.py"]);
    assert_eq!(
        owners(&blame),
        vec![
            some("write the calculator"),
            some("write the calculator"),
            some("write the calculator"),
            some("write the calculator"),
            // Changed by `sed` in the second turn.
            some("make sub safer and generate the table"),
        ]
    );
    // Written by a generator the agent ran, not by any edit tool.
    let table = w.json(&["blame", "table.py"]);
    assert_eq!(
        owners(&table),
        vec![some("make sub safer and generate the table"); 3]
    );
}

#[test]
fn a_person_s_edits_between_turns_are_not_the_agent_s() {
    let mut w = calculator_world();
    w.sh("echo 'def mul(a, b): return a * b' >> calc.py");
    w.prompt("p3", "say hello");
    w.stop("p3");
    let lines = owners(&w.json(&["blame", "calc.py"]));
    assert_eq!(lines.last().unwrap(), &None);
    // And what existed before loch was installed belongs to nobody.
    assert_eq!(owners(&w.json(&["blame", "notes.txt"])), vec![None]);
}

#[test]
fn the_link_survives_a_squash_and_a_fresh_clone() {
    let w = calculator_world();
    w.git(&w.repo, &["add", "-A"]);
    w.git(&w.repo, &["commit", "-q", "-m", "squashed"]);
    let clone = w.root.join("teammate");
    w.git(&w.root, &["clone", "-q", &s(&w.repo), &s(&clone)]);
    // Another machine: its own empty HOME, only the store in common.
    let other_home = w.root.join("other-home");
    fs::create_dir_all(&other_home).unwrap();
    let out = w
        .command(env!("CARGO_BIN_EXE_loch"), &clone)
        .args(["blame", "calc.py", "--json"])
        .env("HOME", &other_home)
        .output()
        .unwrap();
    let blame = json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    assert_eq!(
        owners(&blame)[4],
        some("make sub safer and generate the table")
    );
    let first = &blame.get("conversations").unwrap().as_array().unwrap()[0];
    assert!(
        first.get("available").unwrap().truthy(),
        "the transcript comes from the store"
    );
}

#[test]
fn reformatting_keeps_the_link_and_rewriting_loses_it() {
    let w = calculator_world();
    w.sh("sed -i.bak 's/^    return a + b/        return a + b/' calc.py && rm calc.py.bak");
    w.sh("sed -i.bak 's/^def add(a, b):/def add(x, y):/' calc.py && rm calc.py.bak");
    let lines = owners(&w.json(&["blame", "calc.py"]));
    assert_eq!(lines[0], None, "rewritten by hand, so no longer the turn's");
    assert_eq!(
        lines[1],
        some("write the calculator"),
        "re-indented, still the turn's"
    );
}

#[test]
fn stopping_twice_for_one_prompt_is_one_turn() {
    let mut w = World::new(true);
    w.prompt("p1", "two steps");
    w.write("a.py", "first = 1\n");
    w.stop("p1");
    w.write("a.py", "first = 1\nsecond = 2\n");
    // Claude Code stops again for the same prompt.
    let out = w.loch_in(&w.repo, &["hook", "stop"], &w.event());
    assert!(out.status.success());
    let log = w.json(&["log", "a.py"]);
    assert_eq!(
        log.get("conversations").unwrap().as_array().unwrap().len(),
        1
    );
    assert_eq!(
        owners(&w.json(&["blame", "a.py"])),
        vec![some("two steps"); 2]
    );
}

#[test]
fn a_turn_with_no_baseline_attributes_nothing_and_becomes_one() {
    let mut w = World::new(true);
    w.write("old.py", "existing_code = True\n");
    // A stop with no prompt hook before it and no earlier snapshot.
    w.record(
        r#"{"type": "user", "uuid": "p0", "message": {"content": "hooks just installed"}}"#.into(),
    );
    let out = w.loch_in(&w.repo, &["hook", "stop"], &w.event());
    assert!(out.status.success());
    assert_eq!(owners(&w.json(&["blame", "old.py"])), vec![None]);
    // The next turn measures from that snapshot.
    w.prompt("p1", "change it");
    w.write("old.py", "existing_code = False\n");
    w.stop("p1");
    assert_eq!(
        owners(&w.json(&["blame", "old.py"])),
        vec![some("change it")]
    );
}

#[test]
fn ignored_files_are_not_recorded() {
    let mut w = World::new(true);
    w.write(".gitignore", "build/\n");
    w.prompt("p1", "build it");
    w.write("build/out.txt", "generated artifact line\n");
    w.write("src.py", "source_line = 1\n");
    w.stop("p1");
    let records: Vec<_> = fs::read_dir(w.store.join("index/file"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(records.iter().any(|name| name.ends_with("src.py")));
    assert!(
        !records.iter().any(|name| name.contains("build")),
        "{:?}",
        records
    );
}

#[test]
fn a_session_in_the_home_directory_is_not_snapshotted() {
    let w = World::new(false);
    let event = format!(
        r#"{{"session_id": "{}", "cwd": "{}", "transcript_path": "{}"}}"#,
        SESSION,
        s(&w.home),
        s(&w.transcript)
    );
    for name in ["prompt", "stop"] {
        let out = w.loch_in(&w.home, &["hook", name], &event);
        assert!(out.status.success() && out.stdout.is_empty());
    }
    assert!(!w.home.join(".loch/objects").exists());
}

#[test]
fn hooks_never_fail_and_never_print() {
    let w = World::new(true);
    for (name, stdin) in [
        ("prompt", "not json"),
        ("stop", "[1, 2]"),
        ("stop", "{}"),
        ("prompt", r#"{"session_id": "../escape"}"#),
        ("pre-tool", "{}"),
        ("bogus", "{}"),
    ] {
        let out = w.loch_in(&w.repo, &["hook", name], stdin);
        assert!(out.status.success(), "{} {}", name, stdin);
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
}

#[test]
fn stop_snapshots_the_transcript_into_the_store() {
    let mut w = World::new(true);
    w.prompt("p1", "anything");
    w.stop("p1");
    let key = w
        .store
        .join("transcripts/claude-code/-repo")
        .join(format!("{}.jsonl", SESSION));
    assert_eq!(fs::read(key).unwrap(), fs::read(&w.transcript).unwrap());
}

#[test]
fn log_lists_every_turn_that_changed_a_file_or_a_range() {
    let w = calculator_world();
    let whole = w.json(&["log", "calc.py"]);
    let prompts: Vec<&str> = whole
        .get("conversations")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.get("prompt").unwrap().as_str().unwrap())
        .collect();
    assert_eq!(
        prompts,
        vec![
            "make sub safer and generate the table",
            "write the calculator"
        ]
    );
    let range = w.json(&["log", "calc.py:1-2"]);
    assert_eq!(
        range
            .get("conversations")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let past = w.json(&["log", "calc.py:9"]);
    assert!(past.get("code").unwrap().is_str("usage"));
}

#[test]
fn ask_forks_at_the_end_of_the_turn_that_wrote_the_line() {
    let w = calculator_world();
    let asked = w.json(&["ask", "calc.py:5", "--at-turn"]);
    assert!(asked.get("ok").unwrap().truthy());
    assert!(asked.get("at").unwrap().is_str("turn"));
    let fork = asked.get("fork").unwrap();
    let path = fork.get("path").unwrap().as_str().unwrap();
    let text = fs::read_to_string(path).unwrap();
    // Cut at the second turn's last record, so its prompt and reply are in
    // and nothing after them is.
    assert!(text.contains("p2-end"));
    assert!(text.contains("\"customTitle\""));
    let unlinked = w.json(&["ask", "notes.txt:1"]);
    assert!(unlinked.get("code").unwrap().is_str("not-linked"));
}

#[test]
fn ask_finds_a_deleted_transcript_in_the_store_or_says_it_is_missing() {
    let w = calculator_world();
    fs::remove_file(&w.transcript).unwrap();
    assert!(w.json(&["ask", "calc.py:1"]).get("ok").unwrap().truthy());
    fs::remove_dir_all(w.store.join("transcripts")).unwrap();
    fs::remove_dir_all(w.home.join(".loch/cache")).unwrap();
    let missing = w.json(&["ask", "calc.py:1"]);
    assert!(missing.get("code").unwrap().is_str("transcript-missing"));
}

#[test]
fn reindex_rebuilds_the_file_index_from_the_records() {
    let w = calculator_world();
    fs::remove_dir_all(w.store.join("index/file")).unwrap();
    let result = w.json(&["reindex"]);
    assert_eq!(result.get("fileEntries").unwrap().as_i64(), Some(2));
    // Blame from a machine with no local records relies on that index.
    fs::remove_dir_all(w.home.join(".loch/edits")).unwrap();
    assert_eq!(owners(&w.json(&["blame", "table.py"])).len(), 3);
    assert!(owners(&w.json(&["blame", "table.py"]))[0].is_some());
}

#[test]
fn blame_html_writes_one_self_contained_page() {
    let w = calculator_world();
    let page = w.root.join("page.html");
    let result = w.json(&["blame", "calc.py", "--html", &s(&page), "--no-open"]);
    assert!(result.get("ok").unwrap().truthy());
    let html = fs::read_to_string(page).unwrap();
    assert!(html.contains("write the calculator"));
    assert!(!html.contains("<script src"), "no external assets");
}

#[test]
fn a_workspace_without_git_is_attributed_too() {
    let mut w = World::new(false);
    w.prompt("p1", "plain directory");
    w.write("sub/x.txt", "plain directory content\n");
    w.stop("p1");
    let out = w.loch_in(&w.repo.join("sub"), &["blame", "x.txt", "--json"], "");
    let blame = json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    assert_eq!(owners(&blame), vec![some("plain directory")]);
    assert!(blame
        .get("project")
        .unwrap()
        .as_str()
        .unwrap()
        .starts_with("dir:"));
}

#[test]
fn a_person_s_lone_brace_is_not_credited_to_the_agent() {
    let mut w = World::new(true);
    w.prompt("p1", "write a block");
    w.write("a.js", "function go() {\n  return compute();\n}\n");
    w.stop("p1");
    // Written by hand afterwards: the same text as the agent's closing
    // brace, but on its own it proves nothing.
    w.sh("printf 'const other = 1;\\n}\\n' >> a.js");
    let lines = owners(&w.json(&["blame", "a.js"]));
    assert_eq!(lines[..3], vec![some("write a block"); 3][..]);
    assert_eq!(lines[3..], [None, None]);
}

fn interrupted_flags(w: &World) -> Vec<bool> {
    let record = fs::read_to_string(
        w.home
            .join(".loch/edits/claude-code")
            .join(format!("{}.json", SESSION)),
    )
    .unwrap();
    json::loads(&record)
        .unwrap()
        .get("turns")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.get("interrupted").unwrap().truthy())
        .collect()
}

#[test]
fn a_cancelled_turn_is_closed_by_the_next_prompt() {
    let mut w = World::new(true);
    w.prompt("p1", "start something");
    w.write("half.py", "half_done = True\n");
    // Cancelled: Claude Code runs no Stop. The next prompt arrives instead.
    w.prompt("p2", "do something else");
    w.write("other.py", "other_work = 2\n");
    w.stop("p2");
    // The cancelled turn's edits are its own, not the next turn's baseline.
    assert_eq!(
        owners(&w.json(&["blame", "half.py"])),
        vec![some("start something")]
    );
    assert_eq!(
        owners(&w.json(&["blame", "other.py"])),
        vec![some("do something else")]
    );
    assert_eq!(interrupted_flags(&w), vec![true, false]);
}

#[test]
fn quitting_mid_turn_is_closed_by_the_session_s_end() {
    let mut w = World::new(true);
    w.prompt("p1", "leave halfway");
    w.write("left.py", "left_behind = 1\n");
    w.hook("end");
    assert_eq!(
        owners(&w.json(&["blame", "left.py"])),
        vec![some("leave halfway")]
    );
    assert_eq!(interrupted_flags(&w), vec![true]);
    assert!(
        !w.session_state().exists(),
        "the session's state is released"
    );
}

#[test]
fn a_crash_then_resume_closes_the_turn_on_the_next_prompt() {
    let mut w = World::new(true);
    w.prompt("p1", "before the crash");
    w.write("crashed.py", "written_before_crash = 1\n");
    // The process dies: no Stop, no SessionEnd. `claude --resume` keeps the
    // session id, so its next prompt finds the turn still open.
    w.prompt("p2", "after resuming");
    w.stop("p2");
    assert_eq!(
        owners(&w.json(&["blame", "crashed.py"])),
        vec![some("before the crash")]
    );
}

#[test]
fn collection_keeps_what_the_next_turn_needs_and_drops_the_rest() {
    let mut w = World::new(true);
    for turn in 0..4 {
        w.prompt(&format!("p{}", turn), &format!("turn {}", turn));
        w.write(
            "grow.py",
            &(0..=turn)
                .map(|i| format!("line_number_{} = {}\n", i, i))
                .collect::<String>(),
        );
        w.stop(&format!("p{}", turn));
    }
    let before = w.objects();
    w.age_everything();
    let collected = w.json(&["gc"]);
    let removed = collected.get("removed").unwrap().as_i64().unwrap();
    assert!(removed > 0, "earlier turns' snapshots go: {:?}", collected);
    assert_eq!(w.objects(), before - removed as usize);
    // Blame never needed them, and the next turn's baseline survived.
    assert_eq!(owners(&w.json(&["blame", "grow.py"]))[0], some("turn 0"));
    w.prompt("p9", "after collecting");
    w.write("grow.py", "line_number_0 = 0\nbrand_new_line = 9\n");
    w.stop("p9");
    assert_eq!(
        owners(&w.json(&["blame", "grow.py"]))[1],
        some("after collecting")
    );
}

#[test]
fn an_open_turn_survives_collection_until_it_is_abandoned() {
    let mut w = World::new(true);
    w.prompt("p1", "still running");
    w.write("slow.py", "slow_work = 1\n");
    // Another session in the same workspace moves its latest snapshot on, so
    // only this turn's own state still points at where it began.
    let other = format!(
        r#"{{"session_id": "99999999-0000-0000-0000-000000000000", "cwd": "{}"}}"#,
        s(&w.repo)
    );
    assert!(w
        .loch_in(&w.repo, &["hook", "prompt"], &other)
        .status
        .success());
    // Old objects, but the turn is open: its start snapshot is a root.
    w.age(".loch/objects");
    w.json(&["gc"]);
    w.stop("p1");
    assert_eq!(
        owners(&w.json(&["blame", "slow.py"])),
        vec![some("still running")]
    );

    // A crash nobody resumes: once the state is past ABANDONED it lets go.
    w.prompt("p2", "never finished");
    w.age_everything();
    w.json(&["gc"]);
    assert!(!w.session_state().exists());
}

#[test]
fn a_killed_write_leaves_nothing_half_written_behind() {
    let mut w = World::new(true);
    w.prompt("p1", "anything");
    w.stop("p1");
    // What a hook killed between write and rename leaves: an aside file.
    let objects = w.home.join(".loch/objects/ab");
    fs::create_dir_all(&objects).unwrap();
    fs::write(objects.join(".cdef.tmp999"), "partial").unwrap();
    w.age_everything();
    w.json(&["gc"]);
    assert!(!objects.join(".cdef.tmp999").exists());
}

#[test]
fn the_hooks_collect_on_their_own_once_due() {
    let mut w = World::new(true);
    w.prompt("p1", "first");
    w.stop("p1");
    let counter = w.home.join(".loch/snapshots-since-collect");
    fs::write(&counter, "49").unwrap();
    w.prompt("p2", "second");
    // That prompt's snapshot made 50: the hook collected and reset the count.
    assert_eq!(fs::read_to_string(&counter).unwrap().trim(), "0");
}
