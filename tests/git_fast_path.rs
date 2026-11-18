//! The native git fast path (`git::head`, `git::absolute_git_dir`,
//! `Layout::discover`) must give exactly git's answer on every layout, and
//! decline — rather than guess — on the ones it doesn't understand.
//!
//! `Layout::discover` reads discovery variables from the process environment,
//! so every test holds `LOCK`; the one that sets `GIT_DIR` removes it again
//! before letting go.

use loch::git::{self, Layout};
use loch::json;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

const DISCOVERY: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_INDEX_FILE",
    "GIT_CONFIG_PARAMETERS",
];

struct World {
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn fixed_env(root: &Path, home: &Path) -> Vec<(String, String)> {
    let mut vars = vec![
        ("HOME".to_string(), s(home)),
        ("GIT_CONFIG_GLOBAL".to_string(), s(&root.join("gitconfig"))),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
    ];
    for name in ["AUTHOR", "COMMITTER"] {
        vars.push((format!("GIT_{}_NAME", name), "Test".into()));
        vars.push((format!("GIT_{}_EMAIL", name), "test@example.com".into()));
        vars.push((format!("GIT_{}_DATE", name), "2026-01-01T00:00:00Z".into()));
    }
    vars
}

fn world() -> World {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = fs::canonicalize(std::env::temp_dir()).unwrap();
    let root = base.join(format!(
        "loch-fastpath-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("repo");
    let home = root.join("home");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(root.join("gitconfig"), "").unwrap();
    // The fallback inside `git::head` spawns git from this process's env.
    for (key, value) in fixed_env(&root, &home) {
        std::env::set_var(key, value);
    }
    for key in DISCOVERY {
        std::env::remove_var(key);
    }
    World {
        root,
        repo,
        home,
        _guard: guard,
    }
}

impl World {
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        for key in DISCOVERY {
            command.env_remove(key);
        }
        command.envs(fixed_env(&self.root, &self.home));
        command
    }

    fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self
            .command("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.repo, args)
    }

    /// `git init` with extra arguments; false when git refuses them.
    fn try_init(&self, extra: &[&str]) -> bool {
        self.command("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["init", "-q", "-b", "main"])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    fn init(&self) {
        assert!(self.try_init(&[]));
    }

    fn commit_in(&self, cwd: &Path, message: &str) {
        self.git_in(cwd, &["commit", "-q", "--allow-empty", "-m", message]);
    }

    fn commit(&self, message: &str) {
        self.commit_in(&self.repo, message);
    }

    /// What git itself says, with `extra` env on the git process.
    fn answers(&self, cwd: &Path, extra: &[(&str, &str)]) -> (Option<String>, String) {
        let run = |args: &[&str]| {
            let output = self
                .command("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .envs(extra.iter().copied())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        let head = run(&["rev-parse", "--verify", "-q", "HEAD"]);
        let git_dir = run(&["rev-parse", "--absolute-git-dir"]);
        assert!(!git_dir.is_empty());
        ((!head.is_empty()).then_some(head), git_dir)
    }

    /// `loch hook pre-tool` with no git on PATH, returning the HEAD it
    /// recorded. Writing the pending file at all needs the git directory, so
    /// a recorded HEAD means both questions were answered natively.
    fn native_pre_tool(&self, cwd: &Path, git_dir: &str, call: &str) -> String {
        let empty = self.root.join("empty-path");
        fs::create_dir_all(&empty).unwrap();
        let event = json::dumps(
            &json::Value::Object(loch::object! {
                "session_id" => "11111111-2222-3333-4444-555555555555",
                "cwd" => s(cwd),
                "tool_name" => "Bash",
                "tool_use_id" => call,
                "tool_input" => loch::object! {"command" => "git commit"},
            }),
            json::DEFAULT,
        );
        let mut child = self
            .command(env!("CARGO_BIN_EXE_loch"))
            .args(["hook", "pre-tool"])
            .env("PATH", &empty)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(event.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let pending = Path::new(git_dir).join("loch").join("pending").join(call);
        let text = fs::read_to_string(&pending)
            .unwrap_or_else(|_| panic!("no pending file at {}: git was needed", s(&pending)));
        json::loads(&text)
            .unwrap()
            .get("head")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// The library's answers equal git's, whichever path produced them.
fn assert_matches_git(w: &World, cwd: &Path) -> (Option<String>, String) {
    let (head, git_dir) = w.answers(cwd, &[]);
    let path = s(cwd);
    assert_eq!(git::head(&path).unwrap(), head, "head at {}", path);
    assert_eq!(git::absolute_git_dir(&path).unwrap(), git_dir);
    (head, git_dir)
}

/// The fast path, and only the fast path, answers — and answers right.
fn assert_native(w: &World, cwd: &Path, call: &str) {
    let (head, git_dir) = assert_matches_git(w, cwd);
    let layout = Layout::discover(&s(cwd)).expect("the fast path declined");
    assert_eq!(s(&layout.git_dir), git_dir);
    assert_eq!(layout.head(), Some(head.clone()));
    assert_eq!(
        w.native_pre_tool(cwd, &git_dir, call),
        head.unwrap_or_default()
    );
}

#[test]
fn plain_repository() {
    let w = world();
    w.init();
    w.commit("first");
    assert_native(&w, &w.repo, "toolu_a");
    w.commit("second");
    assert_native(&w, &w.repo, "toolu_b");
}

#[test]
fn unborn_head() {
    let w = world();
    w.init();
    let (head, _) = assert_matches_git(&w, &w.repo);
    assert_eq!(head, None);
    assert_native(&w, &w.repo, "toolu_a");
    w.commit("root");
    assert_native(&w, &w.repo, "toolu_b");
}

#[test]
fn detached_head() {
    let w = world();
    w.init();
    w.commit("first");
    w.git(&["checkout", "-q", "--detach"]);
    let head_file = fs::read_to_string(w.repo.join(".git/HEAD")).unwrap();
    assert!(!head_file.starts_with("ref: "));
    assert_native(&w, &w.repo, "toolu_a");
    w.commit("detached work");
    assert_native(&w, &w.repo, "toolu_b");
}

#[test]
fn packed_refs() {
    let w = world();
    w.init();
    w.commit("first");
    w.git(&["pack-refs", "--all"]);
    assert!(!w.repo.join(".git/refs/heads/main").exists());
    assert_native(&w, &w.repo, "toolu_a");
    w.commit("after packing");
    w.git(&["pack-refs", "--all"]);
    assert!(!w.repo.join(".git/refs/heads/main").exists());
    assert_native(&w, &w.repo, "toolu_b");
}

#[test]
fn linked_worktree() {
    let w = world();
    w.init();
    w.commit("first");
    let wt = w.root.join("wt1");
    w.git(&["worktree", "add", "-q", "-b", "feature", &s(&wt)]);
    // As the hook event names it in the reference scenario.
    let cwd = wt.join(".");
    assert_native(&w, &cwd, "toolu_a");
    w.commit_in(&wt, "in the worktree");
    assert_native(&w, &cwd, "toolu_b");
    // The main checkout is untouched by the worktree's commit.
    assert_native(&w, &w.repo, "toolu_c");
}

#[test]
fn subdirectory_through_a_symlink() {
    let w = world();
    w.init();
    w.commit("first");
    fs::create_dir_all(w.repo.join("sub").join("deeper")).unwrap();
    std::os::unix::fs::symlink(w.repo.join("sub"), w.root.join("link")).unwrap();
    let cwd = w.root.join("link").join("deeper");
    assert_native(&w, &cwd, "toolu_a");
    w.commit("from a subdirectory");
    assert_native(&w, &cwd, "toolu_b");
}

#[test]
fn reftable_is_left_to_git() {
    let w = world();
    if !w.try_init(&["--ref-format=reftable"]) {
        eprintln!("skipped: this git doesn't support --ref-format=reftable");
        return;
    }
    w.commit("first");
    assert!(Layout::discover(&s(&w.repo)).is_none());
    assert_matches_git(&w, &w.repo);
    w.commit("agent work");
    assert!(Layout::discover(&s(&w.repo)).is_none());
    let (head, _) = assert_matches_git(&w, &w.repo);
    assert!(head.is_some());
}

#[test]
fn git_dir_variable_is_left_to_git() {
    let w = world();
    w.init();
    w.commit("first");
    let other = w.root.join("elsewhere");
    fs::create_dir_all(&other).unwrap();
    let git_dir = s(&w.repo.join(".git"));
    // Without the variable `elsewhere` is no repository at all.
    assert!(Layout::discover(&s(&other)).is_none());

    std::env::set_var("GIT_DIR", &git_dir);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (head, answer_dir) = w.answers(&other, &[("GIT_DIR", git_dir.as_str())]);
        assert!(head.is_some());
        // Even a cwd the fast path would understand is declined.
        assert!(Layout::discover(&s(&w.repo)).is_none());
        assert!(Layout::discover(&s(&other)).is_none());
        assert_eq!(git::head(&s(&other)).unwrap(), head);
        assert_eq!(git::absolute_git_dir(&s(&other)).unwrap(), answer_dir);
    }));
    std::env::remove_var("GIT_DIR");
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
