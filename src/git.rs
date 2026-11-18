//! Git, by subprocess — plus a native fast path for the two questions the
//! hooks ask on every Bash call: where the git directory is and what HEAD is.
//!
//! The fast path exists because a `git` process costs ~8ms and the hooks ask
//! both questions twice per agent command; it is what takes `loch hook` from
//! tens of milliseconds to one. It is only trusted on layouts it fully
//! understands. Anything else — an environment variable that changes
//! discovery, reftable, a repository owned by someone else (`safe.directory`),
//! a ref it can't parse, Windows — returns `None` and the caller asks `git`,
//! so the worst a gap here costs is speed, never a different answer.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
#[allow(dead_code)] // stderr, for the commands that will report it
pub struct GitError(pub String);

pub fn git(cwd: &str, args: &[&str], stdin: Option<&str>) -> Result<String, GitError> {
    let (status, stdout, stderr) = run(cwd, args, stdin)?;
    if !status {
        let message = stderr.trim();
        return Err(GitError(if message.is_empty() {
            "git failed".into()
        } else {
            message.into()
        }));
    }
    Ok(stdout)
}

/// Runs git and returns its stdout whatever the exit status, as Python's
/// `git(..., check=False)` does.
pub fn git_unchecked(cwd: &str, args: &[&str]) -> Result<String, GitError> {
    run(cwd, args, None).map(|(_, stdout, _)| stdout)
}

fn run(cwd: &str, args: &[&str], stdin: Option<&str>) -> Result<(bool, String, String), GitError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command
        .spawn()
        .map_err(|error| GitError(error.to_string()))?;
    if let Some(input) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(input.as_bytes());
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| GitError(error.to_string()))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// `git rev-parse --verify -q HEAD`, or `None` on an unborn branch or outside
/// a repository.
pub fn head(cwd: &str) -> Result<Option<String>, GitError> {
    if let Some(layout) = Layout::discover(cwd) {
        if let Some(answer) = layout.head() {
            return Ok(answer);
        }
    }
    let out = git_unchecked(cwd, &["rev-parse", "--verify", "-q", "HEAD"])?;
    let sha = out.trim();
    Ok(if sha.is_empty() {
        None
    } else {
        Some(sha.into())
    })
}

/// `git rev-parse --absolute-git-dir`.
pub fn absolute_git_dir(cwd: &str) -> Result<String, GitError> {
    if let Some(layout) = Layout::discover(cwd) {
        return Ok(layout.git_dir.to_string_lossy().into_owned());
    }
    Ok(git(cwd, &["rev-parse", "--absolute-git-dir"], None)?
        .trim()
        .to_string())
}

/// Variables that change how git finds or reads a repository. With any of
/// them set the native answer could differ from git's, so it isn't given.
const DISCOVERY_VARIABLES: &[&str] = &[
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

pub struct Layout {
    pub git_dir: PathBuf,
    common_dir: PathBuf,
}

impl Layout {
    pub fn discover(cwd: &str) -> Option<Layout> {
        if !cfg!(unix) {
            return None;
        }
        if DISCOVERY_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return None;
        }
        let start = fs::canonicalize(cwd).ok()?;
        // Inside a git directory itself git behaves as if GIT_DIR were set.
        if start.components().any(|part| part.as_os_str() == ".git") {
            return None;
        }
        let mut dir: &Path = &start;
        loop {
            let dot_git = dir.join(".git");
            if let Ok(meta) = fs::symlink_metadata(&dot_git) {
                if !owned_by_us(dir) {
                    return None;
                }
                let git_dir = if meta.is_dir() {
                    dot_git
                } else if meta.is_file() {
                    let text = fs::read_to_string(&dot_git).ok()?;
                    let target = text
                        .strip_prefix("gitdir: ")?
                        .trim_end_matches(['\n', '\r']);
                    fs::canonicalize(dir.join(target)).ok()?
                } else {
                    return None;
                };
                if !git_dir.join("HEAD").is_file() {
                    return None;
                }
                let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
                    Ok(text) => fs::canonicalize(git_dir.join(text.trim_end())).ok()?,
                    Err(_) => git_dir.clone(),
                };
                if git_dir.join("reftable").exists() || common_dir.join("reftable").exists() {
                    return None;
                }
                if !common_dir.join("objects").is_dir() {
                    return None;
                }
                return Some(Layout {
                    git_dir,
                    common_dir,
                });
            }
            // A bare repository or a filesystem root: leave those to git.
            if dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
                return None;
            }
            dir = dir.parent()?;
        }
    }

    /// `Some(None)` is a definite unborn HEAD; `None` means "ask git".
    pub fn head(&self) -> Option<Option<String>> {
        let mut text = fs::read_to_string(self.git_dir.join("HEAD")).ok()?;
        for _ in 0..5 {
            let line = text.trim_end_matches(['\n', '\r']);
            let Some(name) = line.strip_prefix("ref: ") else {
                return object_id(line).map(Some);
            };
            let name = name.trim();
            if !name.starts_with("refs/") || name.contains("..") {
                return None;
            }
            match fs::read_to_string(self.ref_base(name).join(name)) {
                Ok(next) => text = next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return self.packed(name);
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// Per-worktree refs live in the worktree's own git directory; everything
    /// else is shared through the common directory.
    fn ref_base(&self, name: &str) -> &Path {
        let per_worktree = ["refs/bisect/", "refs/worktree/", "refs/rewritten/"];
        if per_worktree.iter().any(|prefix| name.starts_with(prefix)) {
            &self.git_dir
        } else {
            &self.common_dir
        }
    }

    fn packed(&self, name: &str) -> Option<Option<String>> {
        let text = match fs::read_to_string(self.common_dir.join("packed-refs")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(None),
            Err(_) => return None,
        };
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((sha, refname)) = line.split_once(' ') {
                if refname == name {
                    return object_id(sha).map(Some);
                }
            }
        }
        Some(None)
    }
}

fn object_id(text: &str) -> Option<String> {
    let valid = (text.len() == 40 || text.len() == 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then(|| text.to_string())
}

/// git's `safe.directory` check refuses a repository owned by another user;
/// answering natively there would answer where git declines to.
#[cfg(unix)]
fn owned_by_us(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    fs::metadata(dir)
        .map(|meta| meta.uid() == uid)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn owned_by_us(_dir: &Path) -> bool {
    false
}
