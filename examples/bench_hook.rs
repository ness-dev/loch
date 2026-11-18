//! Wall-clock cost of `loch hook` per call, for any number of loch binaries.
//!
//!     cargo run --release --example bench_hook -- [--runs N] <loch> [<loch> ...]
//!
//! Each binary runs against its own scratch repository with the event Claude
//! Code sends around a Bash call. Three cases, since they cost different
//! amounts: `pre-tool`, `post-tool` when the call committed nothing (nearly
//! every call), and `post-tool` when it made a commit and a note gets written.
//! Reports the median and p90 in milliseconds.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

const SESSION: &str = "00000000-0000-4000-8000-000000000000";

fn git(repo: &Path, args: &[&str], home: &Path) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "bench@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "bench@example.com")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {:?} failed", args);
}

fn run(loch: &str, name: &str, event: &str, home: &Path) -> f64 {
    let started = Instant::now();
    let mut child = Command::new(loch)
        .args(["hook", name])
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("loch runs");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(event.as_bytes())
        .expect("stdin");
    child.wait().expect("loch exits");
    started.elapsed().as_secs_f64() * 1000.0
}

fn bench(loch: &str, runs: usize) -> Vec<(&'static str, Vec<f64>)> {
    let root = std::env::temp_dir().join(format!("loch-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("scratch dir");
    git(&repo, &["init", "-q", "-b", "main"], &root);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "initial"],
        &root,
    );

    let mut pre = Vec::new();
    let mut post = Vec::new();
    let mut commit = Vec::new();
    for i in 0..runs {
        let event = format!(
            r#"{{"session_id": "{}", "cwd": "{}", "tool_name": "Bash", "tool_input": {{"command": "echo {}"}}, "tool_use_id": "toolu_{:08}"}}"#,
            SESSION,
            repo.display(),
            i,
            i
        );
        pre.push(run(loch, "pre-tool", &event, &root));
        if i % 2 == 1 {
            git(
                &repo,
                &[
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    &format!("change {}", i),
                ],
                &root,
            );
            commit.push(run(loch, "post-tool", &event, &root));
        } else {
            post.push(run(loch, "post-tool", &event, &root));
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    vec![
        ("pre-tool", pre),
        ("post-tool", post),
        ("post-commit", commit),
    ]
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut runs = 40;
    if args.first().map(String::as_str) == Some("--runs") {
        runs = args.get(1).and_then(|n| n.parse().ok()).expect("--runs N");
        args.drain(..2);
    }
    assert!(
        !args.is_empty(),
        "usage: bench_hook [--runs N] <loch> [<loch> ...]"
    );
    println!(
        "{:<48} {:<12} {:>9} {:>9}",
        "binary", "case", "median", "p90"
    );
    for loch in &args {
        for (case, mut samples) in bench(loch, runs) {
            samples.sort_by(|a, b| a.total_cmp(b));
            let median = samples[samples.len() / 2];
            let p90 = samples[(samples.len() * 9 / 10).saturating_sub(1)];
            let label: String = loch
                .chars()
                .rev()
                .take(48)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            println!(
                "{:<48} {:<12} {:>7.1}ms {:>7.1}ms",
                label, case, median, p90
            );
        }
    }
}
