//! Secret scrubbing, as in `loch/redact.py` — best-effort pattern matching
//! and nothing more. The patterns are the Python ones with one translation:
//! Python's `\s` (`str.isspace`, which includes U+001C..U+001F) is spelled out,
//! since the `regex` crate's `\s` is Unicode White_Space and differs from it.
//! fancy-regex supplies the lookarounds; it backtracks like Python's `re`, so
//! leftmost-first alternation picks the same rule at the same place.

use crate::py::SPACE_CLASS;
use fancy_regex::{Regex, RegexBuilder};
use indexmap::IndexMap;
use std::sync::OnceLock;

const START: &str = r"(?<![A-Za-z0-9_-])";
const END: &str = r"(?![A-Za-z0-9_-])";

/// (name, pattern) in priority order: anthropic-key before openai-key, which
/// would otherwise claim the same `sk-` prefix.
fn rules() -> Vec<(&'static str, String)> {
    let env_assignment = format!(
        concat!(
            r"(?P<env_assignment_keep>",
            r"[A-Z0-9_]*",
            r"(?:AWS_SECRET_ACCESS_KEY|SECRET|TOKEN|PASSWORD|PASSWD|API_KEY|PRIVATE_KEY)",
            r"[A-Z0-9_]*",
            r#"["']?[{s}]*[=:][{s}]*["']?"#,
            r")",
            r#"(?!\[REDACTED:)(?![/~.])[^{s}"',;\\]{{8,}}"#
        ),
        s = SPACE_CLASS
    );
    vec![
        (
            "aws-access-key",
            format!(r"{START}(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{{16}}{END}"),
        ),
        (
            "github-token",
            format!(r"{START}gh[pousr]_[A-Za-z0-9]{{36,}}"),
        ),
        (
            "slack-token",
            format!(r"{START}xox[abprs]-[A-Za-z0-9-]{{10,}}"),
        ),
        ("stripe-key", format!(r"{START}sk_live_[A-Za-z0-9]{{24,}}")),
        (
            "anthropic-key",
            format!(r"{START}sk-ant-[A-Za-z0-9_-]{{20,}}"),
        ),
        (
            "openai-key",
            format!(r"{START}sk-(?:proj-)?[A-Za-z0-9_-]{{20,}}"),
        ),
        (
            "google-api-key",
            format!(r"{START}AIza[0-9A-Za-z_-]{{35}}{END}"),
        ),
        // The body excludes the double quote so a BEGIN and an END in two
        // different JSONL records can't be joined into one match.
        (
            "private-key-block",
            r#"-----BEGIN [A-Z ]*PRIVATE KEY-----[^"]*?-----END [A-Z ]*PRIVATE KEY-----"#.into(),
        ),
        (
            "jwt",
            format!(r"{START}eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"),
        ),
        ("env-assignment", env_assignment),
    ]
}

struct Compiled {
    regex: Regex,
    names: Vec<&'static str>,
}

fn compiled() -> &'static Compiled {
    static COMPILED: OnceLock<Compiled> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let rules = rules();
        let pattern = rules
            .iter()
            .map(|(name, pattern)| format!("(?P<{}>{})", name.replace('-', "_"), pattern))
            .collect::<Vec<_>>()
            .join("|");
        Compiled {
            // Python's `re` has no backtracking limit; a high one keeps a
            // pathological transcript slow rather than failing it.
            regex: RegexBuilder::new(&pattern)
                .backtrack_limit(usize::MAX / 2)
                .build()
                .expect("the redaction rules compile"),
            names: rules.iter().map(|(name, _)| *name).collect(),
        }
    })
}

#[derive(Debug)]
pub struct RedactionError(pub String);

pub struct Redaction {
    pub text: String,
    /// Rule name → count, in order of each rule's first match.
    pub counts: IndexMap<String, usize>,
}

fn matched_rule(captures: &fancy_regex::Captures, names: &[&'static str]) -> &'static str {
    names
        .iter()
        .find(|name| captures.name(&name.replace('-', "_")).is_some())
        .expect("a match belongs to a rule")
}

pub fn scan(text: &str) -> IndexMap<String, usize> {
    let compiled = compiled();
    let mut counts = IndexMap::new();
    for captures in compiled.regex.captures_iter(text).flatten() {
        *counts
            .entry(matched_rule(&captures, &compiled.names).to_string())
            .or_insert(0) += 1;
    }
    counts
}

pub fn redact(text: &str) -> Result<Redaction, RedactionError> {
    let compiled = compiled();
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for captures in compiled.regex.captures_iter(text).flatten() {
        let whole = captures.get(0).expect("group 0 exists");
        let name = matched_rule(&captures, &compiled.names);
        *counts.entry(name.to_string()).or_insert(0) += 1;
        let kept = captures
            .name(&format!("{}_keep", name.replace('-', "_")))
            .map(|m| m.as_str())
            .unwrap_or("");
        out.push_str(&text[last..whole.start()]);
        out.push_str(kept);
        out.push_str(&format!("[REDACTED:{}]", name));
        last = whole.end();
    }
    out.push_str(&text[last..]);

    let leftover = scan(&out);
    if !leftover.is_empty() {
        return Err(RedactionError(format!(
            "secrets still match after redaction: {}",
            detail(&leftover)
        )));
    }
    Ok(Redaction { text: out, counts })
}

fn detail(counts: &IndexMap<String, usize>) -> String {
    let mut items: Vec<(&String, &usize)> = counts.iter().collect();
    items.sort();
    items
        .iter()
        .map(|(name, count)| format!("{} ×{}", name, count))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn summarize(counts: &IndexMap<String, usize>) -> String {
    if counts.is_empty() {
        return String::new();
    }
    let total: usize = counts.values().sum();
    format!(
        "redacted {} secret{} ({})",
        total,
        if total == 1 { "" } else { "s" },
        detail(counts)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_and_keeps_the_assignment_name() {
        let secret = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let text = format!("x {} AWS_SECRET_ACCESS_KEY=abcdefgh12345678 y", secret);
        let result = redact(&text).ok().unwrap();
        assert_eq!(
            result.text,
            "x [REDACTED:github-token] AWS_SECRET_ACCESS_KEY=[REDACTED:env-assignment] y"
        );
        assert_eq!(
            summarize(&result.counts),
            "redacted 2 secrets (env-assignment ×1, github-token ×1)"
        );
        assert!(redact(&result.text).ok().unwrap().counts.is_empty());
    }

    #[test]
    fn a_word_boundary_is_required() {
        assert!(scan(&format!("xAKIA{}", "ABCDEFGHIJKLMNOP")).is_empty());
        assert_eq!(scan(&format!("AKIA{}", "ABCDEFGHIJKLMNOP")).len(), 1);
    }
}
