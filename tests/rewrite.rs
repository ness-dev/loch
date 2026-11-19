use loch::rewrite::{foreign_cwds, residual_origin_paths, rewrite_transcript, RewriteSpec};

fn spec(origin_cwd: &str, origin_home: &str, target_cwd: &str, target_home: &str) -> RewriteSpec {
    RewriteSpec {
        origin_cwd: origin_cwd.into(),
        origin_home: origin_home.into(),
        target_cwd: target_cwd.into(),
        target_home: target_home.into(),
        origin_session_id: None,
        target_session_id: None,
    }
}

fn cross_machine() -> RewriteSpec {
    spec(
        "/Users/mike/proj",
        "/Users/mike",
        "/Users/alice/work/proj",
        "/Users/alice",
    )
}

// Harness puts every worktree at `<repo>-worktrees/<branch>`, a sibling of the
// repo, so a session whose cwd is the main repo shares a prefix with every
// worktree path in the same transcript.
fn harness_layout() -> RewriteSpec {
    spec(
        "/Users/mike/apps/harness",
        "/Users/mike",
        "/private/tmp/dest-worktrees/feat/send",
        "/Users/mike",
    )
}

/// `json.dumps({key: value}, separators=(",", ":"))` for one string field.
fn field(key: &str, value: &str) -> String {
    let mut out = String::from("{");
    loch::json::write_str(&mut out, key, true);
    out.push(':');
    loch::json::write_str(&mut out, value, true);
    out.push('}');
    out
}

#[test]
fn rewrites_the_cwd_field_and_paths_inside_tool_results() {
    let raw = [
        r#"{"type":"user","cwd":"/Users/mike/proj","gitBranch":"main"}"#,
        r#"{"type":"assistant","toolUseResult":"read /Users/mike/proj/src/a.py"}"#,
    ]
    .join("\n");

    let out = rewrite_transcript(&raw, &cross_machine());

    assert!(out.contains(r#""cwd":"/Users/alice/work/proj""#));
    assert!(out.contains("/Users/alice/work/proj/src/a.py"));
    assert!(!out.contains("/Users/mike"));
}

#[test]
fn does_not_double_rewrite_a_cwd_nested_under_the_home_directory() {
    let raw = r#"{"cwd":"/Users/mike/proj","config":"/Users/mike/.claude/settings.json"}"#;

    let out = rewrite_transcript(raw, &cross_machine());

    assert!(out.contains(r#""cwd":"/Users/alice/work/proj""#));
    assert!(out.contains("/Users/alice/.claude/settings.json"));
    assert!(!out.contains("/Users/alice/work/proj/.claude/settings.json"));
}

#[test]
fn rewrites_the_encoded_project_directory_form() {
    let raw = field("note", "~/.claude/projects/-Users-mike-proj/abc.jsonl");

    let out = rewrite_transcript(&raw, &cross_machine());

    assert!(out.contains("-Users-alice-work-proj"));
    assert!(!out.contains("-Users-mike-proj"));
}

#[test]
fn rewrites_the_session_id_when_a_new_one_is_minted() {
    let raw = r#"{"sessionId":"old-id","uuid":"x"}"#;
    let spec = RewriteSpec {
        origin_session_id: Some("old-id".into()),
        target_session_id: Some("new-id".into()),
        ..cross_machine()
    };

    let out = rewrite_transcript(raw, &spec);

    assert!(out.contains(r#""sessionId":"new-id""#));
}

#[test]
fn leaves_the_transcript_untouched_when_origin_and_target_match() {
    let same_machine = spec(
        "/Users/mike/proj",
        "/Users/mike",
        "/Users/mike/proj",
        "/Users/mike",
    );
    let raw = field("cwd", "/Users/mike/proj");

    assert_eq!(rewrite_transcript(&raw, &same_machine), raw);
}

#[test]
fn residual_reports_nothing_when_the_rewrite_was_complete() {
    let out = rewrite_transcript(&field("cwd", "/Users/mike/proj"), &cross_machine());
    assert!(residual_origin_paths(&out, &cross_machine()).is_empty());
}

#[test]
fn residual_reports_origin_paths_that_survived() {
    assert_eq!(
        residual_origin_paths("stale /Users/mike/elsewhere/file.py", &cross_machine()),
        vec!["/Users/mike".to_string()]
    );
}

#[test]
fn leaves_a_sibling_directory_that_only_extends_the_cwd() {
    let sibling = field("cwd", "/Users/mike/apps/harness-worktrees/feat/send");

    let out = rewrite_transcript(&sibling, &harness_layout());

    assert_eq!(out, sibling);
}

#[test]
fn a_skipped_sibling_becomes_a_visible_residual() {
    let raw = field("cwd", "/Users/mike/apps/harness-worktrees/feat/send");

    let out = rewrite_transcript(&raw, &harness_layout());

    assert_eq!(
        residual_origin_paths(&out, &harness_layout()),
        vec!["/Users/mike/apps/harness".to_string()]
    );
}

#[test]
fn rewrites_the_real_cwd_while_leaving_its_sibling_alone() {
    let raw = [
        field("cwd", "/Users/mike/apps/harness"),
        field("cwd", "/Users/mike/apps/harness-worktrees/feat/send"),
        field("toolUseResult", "read /Users/mike/apps/harness/src/main.py"),
    ]
    .join("\n");

    let out = rewrite_transcript(&raw, &harness_layout());

    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec![
            field("cwd", "/private/tmp/dest-worktrees/feat/send"),
            field("cwd", "/Users/mike/apps/harness-worktrees/feat/send"),
            field(
                "toolUseResult",
                "read /private/tmp/dest-worktrees/feat/send/src/main.py"
            ),
        ]
    );
}

#[test]
fn leaves_a_home_directory_that_only_extends_the_origin_home() {
    let raw = field("path", "/Users/mikey/notes.txt");

    assert_eq!(rewrite_transcript(&raw, &cross_machine()), raw);
}

#[test]
fn falls_through_to_the_home_pair_when_the_cwd_pair_fails_its_guard() {
    let out = rewrite_transcript(
        &field("path", "/Users/mike/projector/a.py"),
        &cross_machine(),
    );

    assert_eq!(out, field("path", "/Users/alice/projector/a.py"));
}

#[test]
fn rewrites_the_encoded_slug_only_as_a_complete_component() {
    let raw = concat!(
        r#"{"own":"~/.claude/projects/-Users-mike-proj/abc.jsonl","#,
        r#""sibling":"~/.claude/projects/-Users-mike-proj-worktrees-feat-send/d.jsonl"}"#
    );

    let out = rewrite_transcript(raw, &cross_machine());

    assert!(out.contains("projects/-Users-alice-work-proj/abc.jsonl"));
    assert!(out.contains("projects/-Users-mike-proj-worktrees-feat-send/d.jsonl"));
}

#[test]
fn foreign_cwds_is_empty_when_every_record_reaches_the_target() {
    assert!(foreign_cwds(&field("cwd", "/Users/mike/proj"), &cross_machine()).is_empty());
}

/// The failure that reported success: a cwd the spec was not keyed on
/// declines the cwd pair, falls through to the home pair, and lands as a
/// well-formed path on this machine that isn't there. The origin marker is
/// consumed on the way, so residual_origin_paths comes back clean.
#[test]
fn foreign_cwds_catches_what_residual_paths_cannot_see() {
    let raw = [
        field("cwd", "/Users/mike/proj"),
        field("cwd", "/Users/mike/other"),
        field("cwd", "/Users/mike/other"),
    ]
    .join("\n");

    let out = rewrite_transcript(&raw, &cross_machine());

    assert!(out.contains("/Users/alice/other"));
    assert!(residual_origin_paths(&out, &cross_machine()).is_empty());
    let foreign: Vec<(String, usize, String)> = foreign_cwds(&raw, &cross_machine())
        .into_iter()
        .map(|(origin, f)| (origin, f.count, f.restored))
        .collect();
    assert_eq!(
        foreign,
        vec![(
            "/Users/mike/other".to_string(),
            2,
            "/Users/alice/other".to_string()
        )]
    );
}

/// The origin path is the left-hand side of any mapping a caller states, and
/// it survives losslessly only before the rewrite. Recovering it from the
/// restored value would mean assuming a leading target home came from the
/// origin's — right most of the time, silently wrong otherwise.
#[test]
fn foreign_cwds_keys_on_the_origin_rather_than_a_reversal() {
    let raw = field("cwd", "/Users/mike/other");

    let foreign = foreign_cwds(&raw, &cross_machine());
    assert_eq!(foreign.len(), 1);
    let origin = &foreign[0].0;

    assert_eq!(origin, "/Users/mike/other");
    assert!(raw.contains(origin.as_str()));
    assert!(!rewrite_transcript(&raw, &cross_machine()).contains(origin.as_str()));
}

/// `"cwd"` also appears inside tool results — loch's own --json output among
/// them — where it names some other process's directory. Counting those
/// would fire on a restore that was entirely correct.
#[test]
fn foreign_cwds_ignores_a_cwd_nested_in_tool_output() {
    let raw = format!("{{\"cwd\":\"/Users/mike/proj\",\"toolUseResult\":{}}}", {
        let mut nested = String::new();
        loch::json::write_str(&mut nested, &field("cwd", "/somewhere/else"), true);
        nested
    });

    assert!(foreign_cwds(&raw, &cross_machine()).is_empty());
}

#[test]
fn still_rewrites_a_path_at_every_ordinary_terminator() {
    for terminator in ["\"", "/", "\\", " ", ":", ",", "\n", ")", "'", ""] {
        let raw = format!("/Users/mike/proj{terminator}");

        assert_eq!(
            rewrite_transcript(&raw, &cross_machine()),
            format!("/Users/alice/work/proj{terminator}")
        );
    }
}
