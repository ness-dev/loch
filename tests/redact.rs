use indexmap::IndexMap;
use loch::json::{self, Value};
use loch::redact::{redact, scan, summarize, NAMES};

// Structurally valid, obviously synthetic. Nothing here is or was a credential.
fn fake() -> Vec<(&'static str, String)> {
    vec![
        ("aws-access-key", "AKIAZZZZZZZZEXAMPLE0".to_string()),
        ("github-token", format!("ghp_{}", "A0".repeat(20))),
        // Split, and with non-numeric segments, so the literal in this file
        // does not match GitHub's push-protection pattern for a real Slack token.
        (
            "slack-token",
            format!("xoxb-EXAMPLE-NOT-A-REAL-TOKEN-{}", "Z".repeat(12)),
        ),
        ("stripe-key", format!("sk_live_{}", "0".repeat(24))),
        ("anthropic-key", format!("sk-ant-api03-{}", "x".repeat(24))),
        ("openai-key", format!("sk-proj-{}", "y".repeat(24))),
        ("google-api-key", format!("AIza{}", "B".repeat(35))),
        (
            "jwt",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.c2lnbmF0dXJl".to_string(),
        ),
        (
            "private-key-block",
            concat!(
                "-----BEGIN RSA PRIVATE KEY-----\\nMIIBOgIBAAJBAKfake\\n",
                "-----END RSA PRIVATE KEY-----"
            )
            .to_string(),
        ),
    ]
}

fn fake_value(name: &str) -> String {
    fake().into_iter().find(|(n, _)| *n == name).unwrap().1
}

/// `json.dumps(fields, separators=(",", ":"))` for string-valued fields.
fn line(fields: &[(&str, &str)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json::write_str(&mut out, key, true);
        out.push(':');
        json::write_str(&mut out, value, true);
    }
    out.push('}');
    out
}

fn counts(pairs: &[(&str, usize)]) -> IndexMap<String, usize> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn redacted(text: &str) -> loch::redact::Redaction {
    match redact(text) {
        Ok(out) => out,
        Err(e) => panic!("redaction failed: {}", e.0),
    }
}

#[test]
fn each_rule_matches_and_removes_the_secret() {
    for (name, secret) in fake() {
        let out = redacted(&format!("the value is {secret} ok"));

        assert_eq!(out.counts, counts(&[(name, 1)]), "{name}");
        assert!(!out.text.contains(&secret), "{name}");
        assert!(out.text.contains(&format!("[REDACTED:{name}]")), "{name}");
    }
}

#[test]
fn an_env_assignment_keeps_the_name_and_drops_the_value() {
    let out = redacted("AWS_SECRET_ACCESS_KEY=wJalrFAKEfakeFAKEfakeFAKEfakeFAKEfake0000");

    assert_eq!(out.text, "AWS_SECRET_ACCESS_KEY=[REDACTED:env-assignment]");
    assert_eq!(out.counts, counts(&[("env-assignment", 1)]));
}

#[test]
fn an_env_assignment_is_found_inside_a_json_string_value() {
    let out = redacted(&line(&[("GITHUB_TOKEN", "ghs_notarealtokenatall000000")]));

    let parsed = json::loads(&out.text).unwrap();
    let mut expected = json::Map::new();
    expected.insert(
        "GITHUB_TOKEN".into(),
        Value::str("[REDACTED:env-assignment]"),
    );
    assert_eq!(parsed, Value::Object(expected));
}

#[test]
fn redacted_lines_still_parse_as_json() {
    let raw = [
        line(&[
            ("type", "user"),
            (
                "message",
                &format!("my key is {}", fake_value("aws-access-key")),
            ),
        ]),
        line(&[
            ("type", "assistant"),
            ("toolUseResult", &format!("PASSWORD={}", "p".repeat(20))),
        ]),
        line(&[
            ("type", "assistant"),
            ("toolUseResult", &fake_value("private-key-block")),
        ]),
    ]
    .join("\n");

    let out = redacted(&raw);

    for redacted_line in out.text.lines() {
        json::loads(redacted_line).unwrap();
    }
    assert_eq!(out.counts.values().sum::<usize>(), 3);
}

#[test]
fn a_redaction_token_is_not_itself_redacted() {
    let once = redacted(&format!(
        "AWS_SECRET_ACCESS_KEY={} and {}",
        "z".repeat(40),
        fake_value("github-token")
    ));
    let twice = redacted(&once.text);

    assert_eq!(twice.text, once.text);
    assert!(twice.counts.is_empty());
}

#[test]
fn the_longest_match_wins_between_overlapping_prefixes() {
    let out = redacted(&fake_value("anthropic-key"));

    assert_eq!(out.counts, counts(&[("anthropic-key", 1)]));
}

#[test]
fn ordinary_transcript_content_survives_untouched() {
    let innocuous = [
        line(&[("cwd", "/Users/mike/Programming/personal/apps/lochy")]),
        line(&[(
            "message",
            "I refactored rewrite.py to keep the single-pass property.",
        )]),
        line(&[("commit", "c4056e9a1f2b3c4d5e6f708192a3b4c5d6e7f809")]),
        line(&[("uuid", "3f2a1b4c-5d6e-4f70-8192-a3b4c5d6e7f8")]),
        line(&[(
            "note",
            "see https://github.com/ness-dev/loch for the source",
        )]),
        r#"{"usage":{"input_tokens":12345678,"output_tokens":4096}}"#.to_string(),
        line(&[("path", "~/.claude/projects/-Users-mike-proj/abc.jsonl")]),
        line(&[("sha256", &"a".repeat(64))]),
    ]
    .join("\n");

    let out = redacted(&innocuous);

    assert_eq!(out.text, innocuous);
    assert!(out.counts.is_empty());
}

#[test]
fn a_key_prefix_landing_mid_word_is_not_a_key() {
    let out = redacted(
        &[
            line(&[("diff", ".task-container-header { flex: 1; }")]),
            line(&[(
                "blob",
                "PiW6ADDuAAAAAAAAAAAAAAAAAAAAACCAAAAAAAAAAAAAAAAAAAAA",
            )]),
            line(&[("css", "risk-assessment-panel-collapsed-state")]),
        ]
        .join("\n"),
    );

    assert!(out.counts.is_empty());
}

#[test]
fn a_short_or_path_like_value_is_not_treated_as_a_secret() {
    let out = redacted("TOKEN: no\nSECRET_PATH=/Users/mike/.config/creds\nTOKEN_DIR=~/keys");

    assert!(out.counts.is_empty());
}

#[test]
fn scan_reports_what_a_redacted_transcript_no_longer_contains() {
    let raw = format!("{} {}", fake_value("jwt"), fake_value("stripe-key"));

    assert_eq!(scan(&raw), counts(&[("jwt", 1), ("stripe-key", 1)]));
    assert!(scan(&redacted(&raw).text).is_empty());
}

#[test]
fn summarize_names_each_rule_without_quoting_the_secret() {
    assert_eq!(summarize(&IndexMap::new()), "");
    assert_eq!(
        summarize(&counts(&[("jwt", 1)])),
        "redacted 1 secret (jwt ×1)"
    );
    assert_eq!(
        summarize(&counts(&[("env-assignment", 3), ("aws-access-key", 1)])),
        "redacted 4 secrets (aws-access-key ×1, env-assignment ×3)"
    );
}

#[test]
fn every_rule_has_a_token_safe_name() {
    for name in NAMES {
        assert!(!name.contains(['"', '\\', '\n']), "{name}");
    }
}
