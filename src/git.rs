//! Git, by subprocess, for the little loch still asks it: a project's root
//! commit (its identity across clones) and the branch `list` defaults to.
//! Nothing loch records is written to git.

use std::io::Write;
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
