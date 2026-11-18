//! Secret scrubbing, as in `loch/redact.py` — best-effort pattern matching
//! and nothing more. The patterns are the Python ones with one translation:
//! Python's `\s` (`str.isspace`, which includes U+001C..U+001F) is spelled out,
//! since the `regex` crate's `\s` is Unicode White_Space and differs from it.
//!
//! Python runs one alternation of every rule, and a backtracking engine tries
//! every rule at every position — `env-assignment` can start at any capital,
//! which is most of the cost of `save`. The answer is the same if the search
//! is restructured, so it is:
//!
//! - the nine prefixed rules have no lookaround once their one-character
//!   boundary checks (START before, END after a fixed-length body) are made
//!   by hand, so each is searched by the `regex` crate in linear time, whose
//!   leftmost-first semantics are a backtracking engine's;
//! - `env-assignment` is tried, anchored, only at the start of a run of
//!   `[A-Z0-9_]` holding a keyword. Every way it can match from a later
//!   position in the run is also a way from the run's start, so failing there
//!   means failing anywhere in the run, and the run's start is the only
//!   candidate;
//! - the next match overall is the earliest position any rule matches at,
//!   ties going to rule order, scanning resumed after each match — which is
//!   what the one alternation means.
//!
//! The whole Python alternation is kept, under test, as the oracle the fast
//! path is fuzzed against.

use crate::py::SPACE_CLASS;
use indexmap::IndexMap;
use regex::Regex;
use std::sync::OnceLock;

/// The rule names, in priority order: anthropic-key before openai-key, which
/// would otherwise claim the same `sk-` prefix.
const NAMES: [&str; 10] = [
    "aws-access-key",
    "github-token",
    "slack-token",
    "stripe-key",
    "anthropic-key",
    "openai-key",
    "google-api-key",
    "private-key-block",
    "jwt",
    "env-assignment",
];

const ENV: usize = 9;

/// (body, needs START before, needs END after) for the nine prefixed rules.
/// START is `(?<![A-Za-z0-9_-])`, END is `(?![A-Za-z0-9_-])`.
fn prefixed() -> [(&'static str, bool, bool); 9] {
    [
        (r"(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}", true, true),
        (r"gh[pousr]_[A-Za-z0-9]{36,}", true, false),
        (r"xox[abprs]-[A-Za-z0-9-]{10,}", true, false),
        (r"sk_live_[A-Za-z0-9]{24,}", true, false),
        (r"sk-ant-[A-Za-z0-9_-]{20,}", true, false),
        (r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}", true, false),
        (r"AIza[0-9A-Za-z_-]{35}", true, true),
        // The body excludes the double quote so a BEGIN and an END in two
        // different JSONL records can't be joined into one match.
        (
            r#"-----BEGIN [A-Z ]*PRIVATE KEY-----[^"]*?-----END [A-Z ]*PRIVATE KEY-----"#,
            false,
            false,
        ),
        (
            r"eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
            true,
            false,
        ),
    ]
}

/// The value of a named assignment is the only way to find a credential with
/// no distinctive prefix. The lookaheads keep it off filesystem paths and off
/// a token this module already wrote.
fn env_assignment() -> String {
    format!(
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
    )
}

struct Engine {
    rules: Vec<(Regex, bool, bool)>,
    keyword: Regex,
    env: fancy_regex::Regex,
}

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| Engine {
        rules: prefixed()
            .iter()
            .map(|(body, start, end)| (Regex::new(body).expect("rule compiles"), *start, *end))
            .collect(),
        // Every keyword of the alternation contains one of these.
        keyword: Regex::new("SECRET|TOKEN|PASSWORD|PASSWD|API_KEY|PRIVATE_KEY").expect("compiles"),
        env: fancy_regex::RegexBuilder::new(&format!("^(?:{})", env_assignment()))
            .backtrack_limit(usize::MAX / 2)
            .build()
            .expect("env-assignment compiles"),
    })
}

fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn run_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Found {
    start: usize,
    end: usize,
    /// End of `env_assignment_keep`, for the one rule that keeps a prefix.
    keep: usize,
}

impl Engine {
    /// The first match of rule `index` starting at or after `from`.
    fn next(&self, text: &str, index: usize, from: usize) -> Option<Found> {
        if index == ENV {
            return self.next_env(text, from);
        }
        let (regex, start_guard, end_guard) = &self.rules[index];
        let mut at = from;
        while at <= text.len() {
            let found = regex.find_at(text, at)?;
            let before = text[..found.start()].chars().next_back();
            let after = text[found.end()..].chars().next();
            let start_ok = !*start_guard || !before.is_some_and(word);
            let end_ok = !*end_guard || !after.is_some_and(word);
            if start_ok && end_ok {
                return Some(Found {
                    start: found.start(),
                    end: found.end(),
                    keep: found.start(),
                });
            }
            // Python's engine moves on one position, not past the match.
            at = found.start()
                + text[found.start()..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
        }
        None
    }

    fn next_env(&self, text: &str, from: usize) -> Option<Found> {
        let bytes = text.as_bytes();
        let mut at = from;
        loop {
            let keyword = self.keyword.find_at(text, at)?;
            let mut start = keyword.start();
            while start > from && run_char(bytes[start - 1]) {
                start -= 1;
            }
            if let Ok(Some(captures)) = self.env.captures(&text[start..]) {
                let whole = captures.get(0).expect("group 0");
                let keep = captures.name("env_assignment_keep").expect("keep group");
                return Some(Found {
                    start,
                    end: start + whole.end(),
                    keep: start + keep.end(),
                });
            }
            // Failing at a run's start is failing everywhere in it.
            let mut end = keyword.end();
            while end < bytes.len() && run_char(bytes[end]) {
                end += 1;
            }
            at = end;
        }
    }

    /// Every match, as the one alternation's `finditer` would report them.
    fn matches(&self, text: &str) -> Vec<(usize, Found)> {
        let mut cached: [Option<Option<Found>>; 10] = [None; 10];
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let mut best: Option<(usize, Found)> = None;
            for (index, slot) in cached.iter_mut().enumerate() {
                let stale = match slot {
                    None => true,
                    Some(Some(found)) => found.start < cursor,
                    Some(None) => false,
                };
                if stale {
                    *slot = Some(self.next(text, index, cursor));
                }
                if let Some(Some(found)) = slot {
                    if best.is_none_or(|(_, b)| found.start < b.start) {
                        best = Some((index, *found));
                    }
                }
            }
            let Some((index, found)) = best else {
                return out;
            };
            out.push((index, found));
            cursor = found.end;
        }
    }
}

#[derive(Debug)]
pub struct RedactionError(pub String);

pub struct Redaction {
    pub text: String,
    /// Rule name → count, in order of each rule's first match.
    pub counts: IndexMap<String, usize>,
}

pub fn scan(text: &str) -> IndexMap<String, usize> {
    let mut counts = IndexMap::new();
    for (index, _) in engine().matches(text) {
        *counts.entry(NAMES[index].to_string()).or_insert(0) += 1;
    }
    counts
}

pub fn redact(text: &str) -> Result<Redaction, RedactionError> {
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (index, found) in engine().matches(text) {
        let name = NAMES[index];
        *counts.entry(name.to_string()).or_insert(0) += 1;
        out.push_str(&text[last..found.start]);
        out.push_str(&text[found.start..found.keep]);
        out.push_str(&format!("[REDACTED:{}]", name));
        last = found.end;
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

    /// redact.py's one alternation, verbatim but for `\s`: the oracle.
    fn oracle() -> fancy_regex::Regex {
        let start = r"(?<![A-Za-z0-9_-])";
        let end = r"(?![A-Za-z0-9_-])";
        let bodies: Vec<String> = prefixed()
            .iter()
            .map(|(body, s, e)| {
                format!(
                    "{}{}{}",
                    if *s { start } else { "" },
                    body,
                    if *e { end } else { "" }
                )
            })
            .chain(std::iter::once(env_assignment()))
            .collect();
        let pattern = NAMES
            .iter()
            .zip(&bodies)
            .map(|(name, body)| format!("(?P<{}>{})", name.replace('-', "_"), body))
            .collect::<Vec<_>>()
            .join("|");
        fancy_regex::RegexBuilder::new(&pattern)
            .backtrack_limit(usize::MAX / 2)
            .build()
            .unwrap()
    }

    fn oracle_matches(regex: &fancy_regex::Regex, text: &str) -> Vec<(usize, usize, usize, usize)> {
        regex
            .captures_iter(text)
            .map(|c| {
                let c = c.unwrap();
                let index = NAMES
                    .iter()
                    .position(|n| c.name(&n.replace('-', "_")).is_some())
                    .unwrap();
                let whole = c.get(0).unwrap();
                let keep = c
                    .name("env_assignment_keep")
                    .map_or(whole.start(), |k| k.end());
                (index, whole.start(), whole.end(), keep)
            })
            .collect()
    }

    /// Strings built from the pieces the rules care about, so that near
    /// misses, overlaps and boundary cases are common rather than rare.
    #[test]
    fn the_fast_path_agrees_with_the_alternation() {
        let pieces = [
            "AKIA",
            "ASIA",
            "ABCDEFGHIJKLMNOP",
            "0123456789",
            "gh",
            "p_",
            "ghp_",
            "xoxb-",
            "sk-",
            "sk-ant-",
            "sk_live_",
            "proj-",
            "AIza",
            "eyJ",
            ".",
            "-",
            "_",
            "x",
            "A",
            "Z9",
            " ",
            "\t",
            "\u{1c}",
            "\n",
            "=",
            ":",
            "\"",
            "'",
            ",",
            ";",
            "\\",
            "/",
            "~",
            "TOKEN",
            "SECRET",
            "API_KEY",
            "PASSWD",
            "PASSWORD",
            "PRIVATE_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "MY_",
            "[REDACTED:env-assignment]",
            "abcdefghij",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----END RSA PRIVATE KEY-----",
            "é",
            "😀",
            "a1B2c3D4e5a1B2c3D4e5a1B2c3D4e5a1B2c3D4e5",
            "0123456789abcdefghijklmnopqrstuvwxy",
            "eyJhbGci.eyJzdWIi.c2ln",
            "eyJhbGci",
            ".eyJzdWIi",
        ];
        let regex = oracle();
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut matched = [0usize; 10];
        for _ in 0..40_000 {
            let length = 1 + (next() % 24) as usize;
            let text: String = (0..length)
                .map(|_| pieces[(next() % pieces.len() as u64) as usize])
                .collect();
            let fast: Vec<_> = engine()
                .matches(&text)
                .into_iter()
                .map(|(i, f)| (i, f.start, f.end, f.keep))
                .collect();
            assert_eq!(fast, oracle_matches(&regex, &text), "{text:?}");
            for (index, ..) in fast {
                matched[index] += 1;
            }
        }
        // Agreement on texts that match nothing would prove nothing.
        assert!(matched.iter().all(|&n| n > 10), "{matched:?}");
    }

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
