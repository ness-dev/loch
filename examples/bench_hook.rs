//! Wall-clock cost of `loch hook` per turn, for any number of loch binaries.
//!
//!     cargo run --release --example bench_hook -- [--runs N] [--files N] <loch> ...
//!
//! Each binary runs against its own scratch repository of `--files` files
//! (default 2000) with the events Claude Code sends at a turn's boundaries.
//! Two cases: `prompt`, which snapshots, and `stop`, which snapshots, diffs
//! the turn's one changed file and writes the edit record. Reports the median
//! and p90 in milliseconds.

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

fn bench(loch: &str, runs: usize, files: usize) -> Vec<(&'static str, Vec<f64>)> {
    let root = std::env::temp_dir().join(format!("loch-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("scratch dir");
    git(&repo, &["init", "-q", "-b", "main"], &root);
    for i in 0..files {
        let body = format!("fn item_{}() -> usize {{\n    {}\n}}\n", i, i);
        std::fs::write(repo.join(format!("src/f{}.rs", i)), body).expect("write");
    }
    let transcript = root.join("transcript.jsonl");
    let mut prompt = Vec::new();
    let mut stop = Vec::new();
    for i in 0..runs {
        let line = format!(
            r#"{{"type": "user", "uuid": "p{}", "message": {{"content": "turn {}"}}}}"#,
            i, i
        );
        let mut text = std::fs::read_to_string(&transcript).unwrap_or_default();
        text.push_str(&line);
        text.push('\n');
        std::fs::write(&transcript, text).expect("transcript");
        let event = format!(
            r#"{{"session_id": "{}", "cwd": "{}", "transcript_path": "{}"}}"#,
            SESSION,
            repo.display(),
            transcript.display()
        );
        prompt.push(run(loch, "prompt", &event, &root));
        std::fs::write(
            repo.join("src/f0.rs"),
            format!("fn changed() -> usize {{ {} }}\n", i),
        )
        .expect("edit");
        stop.push(run(loch, "stop", &event, &root));
    }
    let _ = std::fs::remove_dir_all(&root);
    vec![("prompt", prompt), ("stop", stop)]
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut runs = 40;
    let mut files = 2000;
    while let Some(flag) = args.first().cloned().filter(|a| a.starts_with("--")) {
        let value: usize = args.get(1).and_then(|n| n.parse().ok()).expect("a number");
        match flag.as_str() {
            "--runs" => runs = value,
            "--files" => files = value,
            _ => panic!("unknown flag {}", flag),
        }
        args.drain(..2);
    }
    assert!(
        !args.is_empty(),
        "usage: bench_hook [--runs N] [--files N] <loch> [<loch> ...]"
    );
    println!(
        "{:<48} {:<12} {:>9} {:>9}",
        "binary", "case", "median", "p90"
    );
    for loch in &args {
        for (case, mut samples) in bench(loch, runs, files) {
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
