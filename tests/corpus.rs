//! Checks against this machine's real transcripts. Opt-in, since the corpus is
//! personal and its size is unbounded:
//! `LOCH_CORPUS=1 cargo test --release --test corpus -- --ignored`, optionally
//! with `LOCH_CORPUS_LIMIT=<n>`. Ported from test/test_corpus.py.
//!
//! Nothing from a transcript is ever asserted on or printed — only session
//! ids and counts, so a failure report is safe to paste.

mod derived_reader;

use loch::claude::{home_dir, projects_dir, read_session_meta, SessionMeta};
use loch::format::pack_derived;
use loch::format::{digest_bytes, Turn};
use loch::ingest::{convert, ingest, Origin};
use loch::json::Map;
use loch::rewrite::{rewrite_transcript, RewriteSpec};
use std::fs;

fn enabled() -> bool {
    if std::env::var("LOCH_CORPUS").is_ok_and(|v| !v.is_empty()) {
        return true;
    }
    eprintln!("set LOCH_CORPUS=1 to run");
    false
}

fn corpus() -> Vec<SessionMeta> {
    let limit: usize = std::env::var("LOCH_CORPUS_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let root = projects_dir(None);
    let mut found = Vec::new();
    let mut directories: Vec<_> = fs::read_dir(&root)
        .expect("projects directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    directories.sort();
    for directory in directories {
        let mut names: Vec<_> = fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".jsonl"))
            .collect();
        names.sort();
        for name in names {
            let path = directory.join(&name);
            let Some(meta) = read_session_meta(path.to_str().expect("UTF-8 path")) else {
                continue;
            };
            found.push(meta);
            if limit != 0 && found.len() >= limit {
                return found;
            }
        }
    }
    found
}

fn ids(turns: &[Turn]) -> Vec<&str> {
    turns.iter().map(|t| t.id.as_str()).collect()
}

/// A deterministic UUID-shaped id per session. Python used uuid5; any stable
/// value distinct from the original serves, since it only feeds the rewrite.
fn new_id(session_id: &str) -> String {
    let hex = digest_bytes(session_id.as_bytes());
    let hex = &hex["sha256:".len()..];
    format!(
        "{}-{}-5{}-8{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

#[test]
#[ignore = "opt-in: LOCH_CORPUS=1, reads this machine's transcripts"]
fn every_session_ingests_and_reads_back_verified() {
    if !enabled() {
        return;
    }
    let home = home_dir();
    let mut failures: Vec<(String, &str)> = Vec::new();
    for meta in corpus() {
        let origin = Origin {
            cwd: meta.cwd.clone(),
            home: home.clone(),
            session_id: meta.session_id.clone(),
        };
        let packed = match ingest(&meta.path, &origin, None).map(|d| pack_derived(&d)) {
            Ok(Ok(packed)) => packed,
            _ => {
                failures.push((meta.session_id, "does not ingest"));
                continue;
            }
        };
        let again = ingest(&meta.path, &origin, None)
            .ok()
            .map(|d| pack_derived(&d));
        if !matches!(again, Some(Ok(ref bytes)) if *bytes == packed) {
            failures.push((meta.session_id, "not idempotent"));
            continue;
        }
        let back = derived_reader::read_packed(&packed)
            .ok()
            .and_then(|d| pack_derived(&d).ok());
        if back.as_deref() != Some(packed.as_slice()) {
            failures.push((meta.session_id, "does not round-trip"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failed: {:?}",
        failures.len(),
        failures
    );
}

/// A transcript that already names the destination paths can't pass: its
/// content is indistinguishable from a rewritten root. Those are skipped
/// rather than failing the run, and counted, to check by hand.
#[test]
#[ignore = "opt-in: LOCH_CORPUS=1, reads this machine's transcripts"]
fn turn_ids_survive_a_restore_to_another_machine() {
    if !enabled() {
        return;
    }
    let (target_cwd, target_home) = ("/srv/loch-corpus/checkout", "/home/loch-corpus");
    let home = home_dir();
    let mut unstable: Vec<String> = Vec::new();
    let mut excused: Vec<String> = Vec::new();
    for meta in corpus() {
        let Ok(raw) = fs::read(&meta.path) else {
            unstable.push(meta.session_id);
            continue;
        };
        // Decoded as ingest decodes it, rather than failing on one bad byte.
        let raw = loch::py::decode_surrogateescape(&raw);
        let new_id = new_id(&meta.session_id);
        let spec = RewriteSpec {
            origin_cwd: meta.cwd.clone(),
            origin_home: home.clone(),
            target_cwd: target_cwd.into(),
            target_home: target_home.into(),
            origin_session_id: Some(meta.session_id.clone()),
            target_session_id: Some(new_id.clone()),
        };
        let source = Origin {
            cwd: meta.cwd.clone(),
            home: home.clone(),
            session_id: meta.session_id.clone(),
        };
        let target = Origin {
            cwd: target_cwd.into(),
            home: target_home.into(),
            session_id: new_id,
        };
        let before = convert(&raw, &source, &Map::new());
        let after = convert(&rewrite_transcript(&raw, &spec), &target, &Map::new());
        let same = match (&before, &after) {
            (Ok(b), Ok(a)) => ids(&b.turns) == ids(&a.turns),
            _ => false,
        };
        if !same {
            if raw.contains(target_cwd) || raw.contains(target_home) {
                excused.push(meta.session_id);
            } else {
                unstable.push(meta.session_id);
            }
        }
    }
    if !excused.is_empty() {
        eprintln!(
            "{} sessions already name the destination paths: {:?}",
            excused.len(),
            excused
        );
    }
    assert!(
        unstable.is_empty(),
        "{} sessions changed turn ids: {:?}",
        unstable.len(),
        unstable
    );
}
