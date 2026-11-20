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
    /// Hooks run as the harness would: every loch line the settings files
    /// hold for the event, in parallel, rather than one `loch hook` call.
    fired: bool,
    sequential: bool,
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
            store: home.join(".loch/store"),
            root,
            home,
            repo,
            minute: 0,
            fired: false,
            sequential: false,
        };
        let bin = world.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_loch"), bin.join("loch")).unwrap();
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
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", s(&self.root.join("bin"))),
            )
            .env_remove("LOCH_STORE")
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
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
        if self.fired {
            self.fire("UserPromptSubmit", &event);
            return;
        }
        let out = self.loch_in(&self.repo, &["hook", "prompt"], &event);
        assert!(out.status.success() && out.stdout.is_empty());
    }

    /// The settings files Claude Code reads for the repo, user-wide first.
    fn settings_files(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".claude/settings.json"),
            self.repo.join(".claude/settings.json"),
            self.repo.join(".claude/settings.local.json"),
        ]
    }

    /// Every command the settings files hold for `event`.
    fn hook_commands(&self, event: &str) -> Vec<String> {
        let mut commands = Vec::new();
        for file in self.settings_files() {
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let settings = json::loads(&text).unwrap();
            let Some(groups) = settings.get("hooks").and_then(|h| h.get(event)) else {
                continue;
            };
            for group in groups.as_array().unwrap() {
                for entry in group.get("hooks").unwrap().as_array().unwrap() {
                    commands.push(entry.get("command").unwrap().as_str().unwrap().to_string());
                }
            }
        }
        commands
    }

    /// Runs an event's hooks as Claude Code does, all at once, in `cwd`; or
    /// one after another, the ordering in which a second `prompt` finds the
    /// first's turn open.
    fn fire_in(&self, cwd: &Path, event: &str, stdin: &str) {
        let one_by_one = self.sequential;
        let mut children = Vec::new();
        for line in self.hook_commands(event) {
            children.push({
                let mut child = self
                    .command("sh", cwd)
                    .args(["-c", &line])
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
                child
            });
            if one_by_one {
                settle(children.pop().unwrap());
            }
        }
        children.into_iter().for_each(settle);
    }

    fn fire(&self, event: &str, stdin: &str) {
        self.fire_in(&self.repo, event, stdin);
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
        if self.fired {
            self.fire("Stop", &self.event());
            return;
        }
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

fn settle(child: std::process::Child) {
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
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
    let first = w.json(&["install", "--project"]);
    assert_eq!(
        first.get("added").unwrap(),
        &json::loads(r#"["UserPromptSubmit", "Stop", "SessionEnd"]"#).unwrap()
    );
    let again = w.json(&["install", "--project"]);
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
    let result = w.json(&["install", "--project"]);
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

/// `loch <args> --json` as another machine: its own HOME, in `cwd`.
fn json_on(w: &World, home: &Path, cwd: &Path, args: &[&str]) -> Value {
    let out = w
        .command(env!("CARGO_BIN_EXE_loch"), cwd)
        .args(args)
        .arg("--json")
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
}

fn pushed(doc: &Value, kind: &str) -> i64 {
    doc.get("pushed")
        .unwrap()
        .get(kind)
        .unwrap()
        .as_i64()
        .unwrap()
}

#[test]
fn the_link_survives_a_squash_and_a_fresh_clone() {
    let w = calculator_world();
    let remote = w.root.join("remote");
    let push = w.json(&["push", &s(&remote)]);
    assert!(push.get("ok").unwrap().truthy(), "{:?}", push);
    w.git(&w.repo, &["add", "-A"]);
    w.git(&w.repo, &["commit", "-q", "-m", "squashed"]);
    let clone = w.root.join("teammate");
    w.git(&w.root, &["clone", "-q", &s(&w.repo), &s(&clone)]);
    // Another machine: its own empty HOME, only the remote in common.
    let other_home = w.root.join("other-home");
    fs::create_dir_all(&other_home).unwrap();
    let before = json_on(&w, &other_home, &clone, &["blame", "calc.py"]);
    assert_eq!(owners(&before)[4], None, "nothing until it pulls");
    let pull = json_on(&w, &other_home, &clone, &["pull", &s(&remote)]);
    assert_eq!(pull.get("records").unwrap().as_i64(), Some(1));
    assert_eq!(pull.get("transcripts").unwrap().as_i64(), Some(1));
    let blame = json_on(&w, &other_home, &clone, &["blame", "calc.py"]);
    assert_eq!(
        owners(&blame)[4],
        some("make sub safer and generate the table")
    );
    let first = &blame.get("conversations").unwrap().as_array().unwrap()[0];
    assert!(
        first.get("available").unwrap().truthy(),
        "the transcript came with the pull"
    );
    let again = json_on(&w, &other_home, &clone, &["pull", &s(&remote)]);
    assert_eq!(again.get("records").unwrap().as_i64(), Some(0));
    assert_eq!(again.get("upToDate").unwrap().as_i64(), Some(1));
}

#[test]
fn push_sends_what_is_new_once() {
    let mut w = calculator_world();
    let remote = w.root.join("remote");
    let dry = w.json(&["push", &s(&remote), "--dry-run"]);
    assert_eq!(pushed(&dry, "records"), 1);
    assert!(!remote.exists(), "a dry run sends nothing");
    let first = w.json(&["push", &s(&remote)]);
    assert_eq!(pushed(&first, "records"), 1);
    assert_eq!(pushed(&first, "fileEntries"), 2);
    assert_eq!(pushed(&first, "transcripts"), 1);
    assert!(remote
        .join(format!("edits/claude-code/{}.json", SESSION))
        .is_file());
    let second = w.json(&["push", &s(&remote)]);
    assert_eq!(second.get("total").unwrap().as_i64(), Some(0));
    assert_eq!(second.get("upToDate").unwrap().as_i64(), Some(4));
    // A new turn changes the record and the transcript, and only those go.
    w.prompt("p3", "one more");
    w.sh("echo 'more_rows = 3' >> table.py");
    w.stop("p3");
    let third = w.json(&["push", &s(&remote)]);
    assert_eq!(pushed(&third, "records"), 1);
    assert_eq!(pushed(&third, "transcripts"), 1);
    assert_eq!(pushed(&third, "fileEntries"), 0);
}

#[test]
fn push_redacts_transcripts_and_leaves_the_local_copy_alone() {
    let mut w = World::new(true);
    w.prompt("p1", "deploy with AKIAABCDEFGHIJKLMNOP please");
    w.write("a.py", "deployed_it = True\n");
    w.stop("p1");
    let remote = w.root.join("remote");
    let push = w.json(&["push", &s(&remote)]);
    assert!(push.get("redacted").unwrap().as_i64().unwrap() >= 1);
    let key = format!("transcripts/claude-code/-repo/{}.jsonl", SESSION);
    let sent = fs::read_to_string(remote.join(&key)).unwrap();
    assert!(!sent.contains("AKIAABCDEFGHIJKLMNOP") && sent.contains("[REDACTED:"));
    let kept = fs::read_to_string(w.store.join(&key)).unwrap();
    assert!(kept.contains("AKIAABCDEFGHIJKLMNOP"));
    assert!(fs::read_to_string(&w.transcript)
        .unwrap()
        .contains("AKIAABCDEFGHIJKLMNOP"));
}

#[test]
fn push_and_pull_cover_this_project_unless_asked_for_all() {
    let w = calculator_world();
    let elsewhere = w.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    w.git(&elsewhere, &["init", "-q", "-b", "main"]);
    w.git(
        &elsewhere,
        &["commit", "-q", "--allow-empty", "-m", "other root"],
    );
    let remote = w.root.join("remote");
    let there = json_on(&w, &w.home, &elsewhere, &["push", &s(&remote)]);
    assert_eq!(there.get("total").unwrap().as_i64(), Some(0));
    assert!(there
        .get("project")
        .unwrap()
        .as_str()
        .unwrap()
        .starts_with("git:"));
    let all = json_on(&w, &w.home, &elsewhere, &["push", &s(&remote), "--all"]);
    assert_eq!(pushed(&all, "records"), 1);
    assert_eq!(all.get("project"), Some(&Value::Null));
    let other_home = w.root.join("other-home");
    fs::create_dir_all(&other_home).unwrap();
    let scoped = json_on(&w, &other_home, &elsewhere, &["pull", &s(&remote)]);
    assert_eq!(scoped.get("records").unwrap().as_i64(), Some(0));
    let everything = json_on(&w, &other_home, &elsewhere, &["pull", &s(&remote), "--all"]);
    assert_eq!(everything.get("records").unwrap().as_i64(), Some(1));
}

#[test]
fn a_remote_tombstone_stops_push_and_pull_forgets() {
    let w = calculator_world();
    let remote = w.root.join("remote");
    w.json(&["push", &s(&remote)]);
    let other_home = w.root.join("other-home");
    fs::create_dir_all(&other_home).unwrap();
    json_on(&w, &other_home, &w.repo, &["pull", &s(&remote)]);
    // Deleted on the remote, as `delete --remote` will.
    for key in [
        format!("edits/claude-code/{}.json", SESSION),
        format!("transcripts/claude-code/-repo/{}.jsonl", SESSION),
    ] {
        fs::remove_file(remote.join(key)).unwrap();
    }
    fs::remove_dir_all(remote.join("index/file")).unwrap();
    let tomb = remote.join(format!("tombstones/session/claude-code/{}", SESSION));
    fs::create_dir_all(tomb.parent().unwrap()).unwrap();
    fs::write(&tomb, "{}").unwrap();
    // The teammate who pulled it can't push it back.
    fs::remove_dir_all(other_home.join(".loch/pushed")).ok();
    let back = json_on(&w, &other_home, &w.repo, &["push", &s(&remote)]);
    assert_eq!(back.get("total").unwrap().as_i64(), Some(0));
    assert!(back.get("tombstoned").unwrap().as_i64().unwrap() >= 1);
    assert!(!remote
        .join("edits")
        .join("claude-code")
        .join(format!("{}.json", SESSION))
        .exists());
    // And a pull forgets loch's copies here, but never the agent's own.
    let pull = w.json(&["pull", &s(&remote)]);
    assert_eq!(pull.get("forgotten").unwrap().as_i64(), Some(1));
    assert!(!w
        .store
        .join(format!("edits/claude-code/{}.json", SESSION))
        .exists());
    assert!(!w
        .home
        .join(format!(".loch/edits/claude-code/{}.json", SESSION))
        .exists());
    assert!(w.transcript.is_file());
    assert_eq!(owners(&w.json(&["blame", "calc.py"]))[4], None);
}

#[test]
fn pull_refuses_what_a_remote_names_badly() {
    let w = World::new(true);
    let remote = w.root.join("remote");
    fs::create_dir_all(remote.join("edits/claude-code")).unwrap();
    fs::create_dir_all(remote.join("edits/nobody")).unwrap();
    fs::write(remote.join("edits/claude-code/bad;id.json"), "{}").unwrap();
    fs::write(remote.join("edits/nobody/s1.json"), "{}").unwrap();
    // A record that claims to be another session than its key.
    let other = "22222222-2222-3333-4444-555555555555";
    fs::write(
        remote.join(format!("edits/claude-code/{}.json", other)),
        format!(
            r#"{{"loch": 1, "agent": "claude-code", "session": "{}", "project": "x", "turns": []}}"#,
            SESSION
        ),
    )
    .unwrap();
    let pull = w.json(&["pull", &s(&remote), "--all"]);
    assert!(pull.get("ok").unwrap().truthy());
    assert_eq!(pull.get("records").unwrap().as_i64(), Some(0));
    assert_eq!(pull.get("refused").unwrap().as_i64(), Some(3));
    assert!(!w.store.join("edits").exists());
}

#[test]
fn push_and_pull_need_a_remote_and_say_so() {
    let w = calculator_world();
    let none = w.json(&["push"]);
    assert!(none.get("code").unwrap().is_str("no-remote"));
    let none = w.json(&["pull"]);
    assert!(none.get("code").unwrap().is_str("no-remote"));
    let unknown = w.json(&["push", "nope"]);
    assert!(unknown.get("code").unwrap().is_str("unknown-remote"));
    let own = w.json(&["push", &s(&w.store)]);
    assert!(own.get("code").unwrap().is_str("invalid-uri"));
    // A named remote, and the default when there is one.
    let remote = w.root.join("remote");
    w.json(&["remote", "add", "origin", &s(&remote)]);
    let push = w.json(&["push"]);
    assert!(push
        .get("remote")
        .unwrap()
        .get("name")
        .unwrap()
        .is_str("origin"));
    assert_eq!(pushed(&push, "records"), 1);
}

#[test]
fn loch_store_still_names_the_remote_with_a_warning() {
    let w = calculator_world();
    let remote = w.root.join("remote");
    let out = w
        .command(env!("CARGO_BIN_EXE_loch"), &w.repo)
        .args(["push"])
        .env("LOCH_STORE", &remote)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("LOCH_REMOTE"));
    let doc = {
        let out = w
            .command(env!("CARGO_BIN_EXE_loch"), &w.repo)
            .args(["push", "--json"])
            .env("LOCH_STORE", &remote)
            .output()
            .unwrap();
        assert!(out.stderr.is_empty());
        json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
    };
    assert!(doc.get("deprecated").unwrap().is_str("LOCH_STORE"));
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
    // Claude Code stops again for the same prompt, after saying more.
    w.record(format!(
        r#"{{"type": "assistant", "uuid": "p1-more", "parentUuid": "p1-end", "sessionId": "{}", "cwd": "{}", "timestamp": "2026-11-18T10:01:45.000Z", "message": {{"role": "assistant", "content": [{{"type": "text", "text": "and another"}}]}}}}"#,
        SESSION,
        s(&w.repo)
    ));
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

/// A remote that accepts connections and never answers, as a hung S3 does.
fn hung_remote() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    (listener, endpoint)
}

#[test]
fn hooks_never_wait_on_a_remote() {
    let mut w = World::new(true);
    let (_listener, endpoint) = hung_remote();
    w.prompt("p1", "write it");
    w.write("a.py", "written_while_the_remote_hangs = 1\n");
    w.record(format!(
        r#"{{"type": "assistant", "uuid": "p1-end", "parentUuid": "p1", "sessionId": "{}", "cwd": "{}", "timestamp": "2026-11-18T10:01:30.000Z", "message": {{"role": "assistant", "content": [{{"type": "text", "text": "done"}}]}}}}"#,
        SESSION,
        s(&w.repo)
    ));
    let started = std::time::Instant::now();
    for name in ["stop", "end"] {
        let mut child = w
            .command(env!("CARGO_BIN_EXE_loch"), &w.repo)
            .args(["hook", name])
            .env("LOCH_STORE", "s3://bucket/prefix")
            .env("LOCH_S3_ENDPOINT", &endpoint)
            .env("AWS_ACCESS_KEY_ID", "test")
            .env("AWS_SECRET_ACCESS_KEY", "test")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(w.event().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
    assert!(owners(&w.json(&["blame", "a.py"]))[0].is_some());
    assert!(w.store.join("transcripts/claude-code").is_dir());
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

// -- install scopes ------------------------------------------------------------------

fn loch_line(name: &str, flags: &str) -> String {
    format!("loch hook {}{} 2>/dev/null || true", name, flags)
}

fn groups_of(path: &Path, event: &str) -> Vec<Value> {
    let settings = json::loads(&fs::read_to_string(path).unwrap()).unwrap();
    settings
        .get("hooks")
        .unwrap()
        .get(event)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn first_command(group: &Value) -> String {
    group.get("hooks").unwrap().as_array().unwrap()[0]
        .get("command")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn install_defaults_to_the_user_s_settings_and_keeps_their_hooks() {
    let w = World::new(true);
    let user = w.home.join(".claude/settings.json");
    fs::create_dir_all(user.parent().unwrap()).unwrap();
    fs::write(
        &user,
        r#"{"theme": "dark", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "their-own"}]}]}}"#,
    )
    .unwrap();
    let installed = w.json(&["install"]);
    assert!(installed.get("scope").unwrap().is_str("user"));
    assert!(installed.get("settings").unwrap().is_str(&s(&user)));
    assert!(installed.get("hookCommand").unwrap().is_str("loch"));
    assert!(!w.repo.join(".claude").exists(), "nothing in the project");
    let written = json::loads(&fs::read_to_string(&user).unwrap()).unwrap();
    assert!(written.get("theme").unwrap().is_str("dark"));
    let stop = groups_of(&user, "Stop");
    assert_eq!(first_command(&stop[0]), "their-own");
    assert_eq!(first_command(&stop[1]), loch_line("stop", " --scope user"));

    let again = w.json(&["install"]);
    assert_eq!(again.get("added").unwrap(), &Value::Array(vec![]));
    assert_eq!(again.get("replaced").unwrap(), &Value::Array(vec![]));
    assert_eq!(groups_of(&user, "Stop").len(), 2);
}

#[test]
fn install_follows_claude_config_dir() {
    let w = World::new(true);
    let account = w.root.join("account");
    let out = w
        .command(env!("CARGO_BIN_EXE_loch"), &w.repo)
        .env("CLAUDE_CONFIG_DIR", &account)
        .args(["install", "--json"])
        .output()
        .unwrap();
    let document = json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    let settings = account.join("settings.json");
    assert!(document.get("settings").unwrap().is_str(&s(&settings)));
    assert!(settings.exists());
    assert!(!w.home.join(".claude/settings.json").exists());
}

#[test]
fn install_rewrites_only_its_own_line_and_in_place() {
    let w = World::new(true);
    let user = w.home.join(".claude/settings.json");
    fs::create_dir_all(user.parent().unwrap()).unwrap();
    // What an older loch, or another path to it, left: in the middle of
    // someone else's hooks.
    fs::write(
        &user,
        r#"{"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "first"}]},
            {"hooks": [{"type": "command", "command": "/old/bin/loch hook stop 2>/dev/null || true"}]},
            {"hooks": [{"type": "command", "command": "last"}]}]}}"#,
    )
    .unwrap();
    let installed = w.json(&["install"]);
    assert_eq!(
        installed.get("replaced").unwrap(),
        &json::loads(r#"["Stop"]"#).unwrap()
    );
    assert_eq!(
        installed.get("added").unwrap(),
        &json::loads(r#"["UserPromptSubmit", "SessionEnd"]"#).unwrap()
    );
    let stop: Vec<String> = groups_of(&user, "Stop").iter().map(first_command).collect();
    assert_eq!(
        stop,
        vec![
            "first".to_string(),
            loch_line("stop", " --scope user"),
            "last".to_string()
        ]
    );
}

#[test]
fn install_takes_one_scope() {
    let w = World::new(true);
    let out = w.loch(&["install", "--local", "--project", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let document = json::loads(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    assert!(document.get("code").unwrap().is_str("usage"));
}

#[test]
fn a_user_wide_hook_does_nothing_outside_a_repository() {
    let mut w = World::new(false);
    w.json(&["install"]);
    w.fired = true;
    w.prompt("p1", "somewhere that is not a project");
    w.write("a.txt", "anything\n");
    w.stop("p1");
    w.fire("SessionEnd", &w.event());
    assert!(
        !w.home.join(".loch").exists(),
        "no snapshot, state or record"
    );
    assert!(!w.store.exists(), "no transcript copied to the store");
}

#[test]
fn a_user_wide_hook_records_in_a_repository() {
    for commits in [true, false] {
        // Without commits there is no root commit to name the project by,
        // and it is still a project.
        let mut w = World::new(commits);
        if !commits {
            w.git(&w.repo, &["init", "-q", "-b", "main"]);
        }
        w.json(&["install"]);
        w.fired = true;
        w.prompt("p1", "write it");
        w.write("a.txt", "the agent's line\n");
        w.stop("p1");
        assert_eq!(owners(&w.json(&["blame", "a.txt"])), vec![some("write it")]);
    }
}

#[test]
fn a_turn_is_recorded_once_beside_a_project_s_own_hooks() {
    for (narrower, sequential) in [("--local", false), ("--local", true), ("--project", true)] {
        let mut w = World::new(true);
        w.sequential = sequential;
        w.json(&["install"]);
        // Ness's install: this worktree alone, with loch's absolute path.
        w.json(&["install", narrower, "--command", env!("CARGO_BIN_EXE_loch")]);
        assert_eq!(w.hook_commands("UserPromptSubmit").len(), 2);
        w.fired = true;
        w.prompt("p1", "first");
        w.write("a.txt", "one\n");
        w.stop("p1");
        w.prompt("p2", "second");
        w.write("b.txt", "two\n");
        w.stop("p2");
        assert_eq!(interrupted_flags(&w), vec![false, false], "{}", narrower);
        assert_eq!(owners(&w.json(&["blame", "a.txt"])), vec![some("first")]);
        assert_eq!(owners(&w.json(&["blame", "b.txt"])), vec![some("second")]);
    }
}

#[test]
fn install_removes_a_second_claude_loch_line() {
    let w = World::new(true);
    let user = w.home.join(".claude/settings.json");
    fs::create_dir_all(user.parent().unwrap()).unwrap();
    // What re-running an older install after loch moved left: two paths.
    fs::write(
        &user,
        r#"{"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "/a/loch hook stop 2>/dev/null || true"}]},
            {"hooks": [{"type": "command", "command": "theirs"},
                       {"type": "command", "command": "/b/loch hook stop 2>/dev/null || true"}]}]}}"#,
    )
    .unwrap();
    let installed = w.json(&["install"]);
    assert_eq!(
        installed.get("removed").unwrap(),
        &json::loads(r#"["Stop"]"#).unwrap()
    );
    let stop = groups_of(&user, "Stop");
    assert_eq!(stop.len(), 2);
    assert_eq!(first_command(&stop[0]), loch_line("stop", " --scope user"));
    assert_eq!(stop[1].get("hooks").unwrap().as_array().unwrap().len(), 1);
    assert_eq!(first_command(&stop[1]), "theirs");
}

/// A genuine second `Stop` can arrive with the same payload and the transcript
/// not yet grown. Past the window it is an event of its own, not another
/// hook's copy of the first, and its edits are the turn's.
#[test]
fn an_identical_stop_after_the_duplicate_window_is_handled() {
    let mut w = World::new(true);
    w.prompt("p1", "two steps");
    w.write("a.py", "first = 1\n");
    w.stop("p1");
    w.write("a.py", "first = 1\nsecond = 2\n");
    std::thread::sleep(std::time::Duration::from_millis(2_500));
    let out = w.loch_in(&w.repo, &["hook", "stop"], &w.event());
    assert!(out.status.success());
    assert_eq!(
        owners(&w.json(&["blame", "a.py"])),
        vec![some("two steps"); 2]
    );
}

fn turn_prompts(record: &Value) -> Vec<String> {
    record
        .get("turns")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.get("promptUuid").unwrap().as_str().unwrap().to_string())
        .collect()
}

fn stored_record(store: &Path) -> Value {
    let text =
        fs::read_to_string(store.join(format!("edits/claude-code/{}.json", SESSION))).unwrap();
    json::loads(&text).unwrap()
}

/// A machine that pulled a session and then recorded turns of it under the
/// same id (a restore that kept the id) has the pulled record in its store and
/// no working copy. Its next turn must add to that record, not replace it.
#[test]
fn a_turn_adds_to_a_pulled_record_rather_than_replacing_it() {
    let mut w = calculator_world();
    fs::remove_dir_all(w.home.join(".loch/edits")).unwrap();
    w.prompt("p3", "after the pull");
    w.sh("echo 'reviewer_line = 1' >> table.py");
    w.stop("p3");
    assert_eq!(
        turn_prompts(&stored_record(&w.store)),
        vec!["p1", "p2", "p3"]
    );
}

/// Two machines recorded different turns of one session. A push merges the
/// remote's copy in rather than writing over it, and keeps the merge here.
#[test]
fn diverged_records_merge_on_push_and_pull() {
    let w = calculator_world();
    let remote = w.root.join("remote");
    w.json(&["push", &s(&remote)]);
    // The other machine's copy: the first turn, and one this machine never saw.
    let theirs = remote.join(format!("edits/claude-code/{}.json", SESSION));
    let mut record = json::loads(&fs::read_to_string(&theirs).unwrap()).unwrap();
    if let Value::Object(map) = &mut record {
        let turns = map.get_mut("turns").unwrap();
        if let Value::Array(list) = turns {
            list.truncate(1);
            let mut extra = list[0].clone();
            if let Value::Object(turn) = &mut extra {
                turn.insert("promptUuid".into(), Value::str("elsewhere"));
                turn.insert("ended".into(), Value::str("2026-11-18T11:00:00.000000Z"));
            }
            list.push(extra);
        }
    }
    fs::write(&theirs, json::dumps(&record, json::DEFAULT)).unwrap();
    // And this machine recorded another turn since its last push.
    let mut w = w;
    w.prompt("p3", "one more here");
    w.sh("echo 'here_line = 3' >> table.py");
    w.stop("p3");

    let push = w.json(&["push", &s(&remote)]);
    assert!(push.get("ok").unwrap().truthy(), "{:?}", push);
    let merged = vec!["p1", "p2", "p3", "elsewhere"];
    assert_eq!(turn_prompts(&stored_record(&remote)), merged);
    assert_eq!(turn_prompts(&stored_record(&w.store)), merged);

    // A third machine that has only its own turn merges both in on pull.
    let other_home = w.root.join("other-home");
    let other_store = other_home.join(".loch/store");
    fs::create_dir_all(other_store.join("edits/claude-code")).unwrap();
    let mut solo = stored_record(&w.store);
    if let Value::Object(map) = &mut solo {
        if let Some(Value::Array(list)) = map.get_mut("turns") {
            list.retain(|t| t.get("promptUuid").unwrap().is_str("p1"));
            let mut mine = list[0].clone();
            if let Value::Object(turn) = &mut mine {
                turn.insert("promptUuid".into(), Value::str("third"));
                turn.insert("ended".into(), Value::str("2026-11-18T12:00:00.000000Z"));
            }
            list.push(mine);
        }
    }
    fs::write(
        other_store.join(format!("edits/claude-code/{}.json", SESSION)),
        json::dumps(&solo, json::DEFAULT),
    )
    .unwrap();
    let pull = json_on(&w, &other_home, &w.repo, &["pull", &s(&remote)]);
    assert_eq!(pull.get("records").unwrap().as_i64(), Some(1));
    assert_eq!(
        turn_prompts(&stored_record(&other_store)),
        vec!["p1", "p2", "p3", "elsewhere", "third"]
    );
}

#[test]
fn push_force_resends_what_the_markers_claim_was_sent() {
    let w = calculator_world();
    let remote = w.root.join("remote");
    w.json(&["push", &s(&remote)]);
    fs::remove_dir_all(&remote).unwrap();
    let stale = w.json(&["push", &s(&remote)]);
    assert_eq!(stale.get("total").unwrap().as_i64(), Some(0));
    let forced = w.json(&["push", &s(&remote), "--force"]);
    assert_eq!(pushed(&forced, "records"), 1);
    assert!(remote
        .join(format!("edits/claude-code/{}.json", SESSION))
        .is_file());
}

#[test]
fn a_symlink_to_the_local_store_is_not_a_remote() {
    let w = calculator_world();
    let link = w.root.join("link-to-store");
    std::os::unix::fs::symlink(&w.store, &link).unwrap();
    let push = w.json(&["push", &s(&link)]);
    assert!(push.get("code").unwrap().is_str("invalid-uri"));
}
