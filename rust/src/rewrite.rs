//! Path rewriting, as in `loch/rewrite.py` — the load-bearing logic.
//!
//! Python builds one alternation regex of escaped literals, each followed by a
//! zero-width guard, and runs one `re.sub`. This is that regex executed by
//! hand: at each position the sources are tried longest first (a stable sort,
//! so equal lengths keep caller order), the first whose text matches *and*
//! whose guard holds wins, and scanning resumes after it, so no substitution's
//! output is ever re-matched. A source that fails its guard falls through to a
//! shorter one at the same position, which is how `/Users/mike/projector`
//! still gets its home rewritten after the cwd pair declines.

use crate::claude::{encode_path, transcript_cwds};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Guard {
    /// `(?![A-Za-z0-9._-])`: a path only matches where it ends on a
    /// component boundary, so a sibling that extends the origin's last
    /// component is left alone. Erring strict is safe: an unsubstituted path
    /// shows up in residual_origin_paths, an over-eager one corrupts silently.
    Path,
    /// `(?![A-Za-z0-9-])`: the encoded slug maps every non-alphanumeric to
    /// `-`, so `<repo>/sub` and `<repo>-sub` encode identically and no
    /// lookahead can tell them apart; it is only rewritten as a whole
    /// component, declining `<encoded-repo>-worktrees-...`.
    EncodedComponent,
    /// The session id is a UUID, not a path.
    None,
}

impl Guard {
    fn holds(self, next: Option<char>) -> bool {
        let Some(c) = next else { return true };
        match self {
            Guard::Path => !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            Guard::EncodedComponent => !(c.is_ascii_alphanumeric() || c == '-'),
            Guard::None => true,
        }
    }
}

pub struct Replacer {
    ordered: Vec<(String, Guard)>,
    lookup: HashMap<String, String>,
    first_bytes: [bool; 256],
}

/// `compile_replacements`. Sorted by length, never by caller order: the
/// single-pass guarantee depends on a nested path matching before the prefix
/// that contains it.
pub fn compile(pairs: &[(String, String, Guard)]) -> Replacer {
    let mut ordered: Vec<(String, Guard)> = Vec::new();
    let mut lookup = HashMap::new();
    let mut first_bytes = [false; 256];
    for (source, dest, guard) in pairs {
        if source.is_empty() || source == dest {
            continue;
        }
        ordered.push((source.clone(), *guard));
        // `{source: dest for ...}`: a repeated source maps to its last dest.
        lookup.insert(source.clone(), dest.clone());
        first_bytes[source.as_bytes()[0] as usize] = true;
    }
    ordered.sort_by_key(|(source, _)| std::cmp::Reverse(source.chars().count()));
    Replacer {
        ordered,
        lookup,
        first_bytes,
    }
}

impl Replacer {
    pub fn apply(&self, text: &str) -> String {
        if self.ordered.is_empty() {
            return text.to_string();
        }
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut i = 0;
        while i < bytes.len() {
            if self.first_bytes[bytes[i] as usize] && text.is_char_boundary(i) {
                let rest = &text[i..];
                let found = self.ordered.iter().find(|(source, guard)| {
                    rest.starts_with(source.as_str())
                        && guard.holds(rest[source.len()..].chars().next())
                });
                if let Some((source, _)) = found {
                    out.push_str(&text[copied..i]);
                    out.push_str(&self.lookup[source]);
                    i += source.len();
                    copied = i;
                    continue;
                }
            }
            i += 1;
        }
        out.push_str(&text[copied..]);
        out
    }
}

#[derive(Clone, Debug)]
pub struct RewriteSpec {
    pub origin_cwd: String,
    pub origin_home: String,
    pub target_cwd: String,
    pub target_home: String,
    pub origin_session_id: Option<String>,
    pub target_session_id: Option<String>,
}

pub fn rewrite_transcript(raw: &str, spec: &RewriteSpec) -> String {
    let mut pairs = vec![
        (
            spec.origin_cwd.clone(),
            spec.target_cwd.clone(),
            Guard::Path,
        ),
        (
            spec.origin_home.clone(),
            spec.target_home.clone(),
            Guard::Path,
        ),
        (
            encode_path(&spec.origin_cwd),
            encode_path(&spec.target_cwd),
            Guard::EncodedComponent,
        ),
    ];
    if let (Some(origin), Some(target)) = (&spec.origin_session_id, &spec.target_session_id) {
        if !origin.is_empty() && !target.is_empty() {
            pairs.push((origin.clone(), target.clone(), Guard::None));
        }
    }
    compile(&pairs).apply(raw)
}

/// Occurrences of the origin's paths that survived a rewrite. Necessary and
/// nowhere near sufficient — see foreign_cwds.
pub fn residual_origin_paths(text: &str, spec: &RewriteSpec) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for needle in [&spec.origin_cwd, &spec.origin_home] {
        if !needle.is_empty()
            && *needle != spec.target_cwd
            && *needle != spec.target_home
            && text.contains(needle.as_str())
            && !found.contains(needle)
        {
            found.push(needle.clone());
        }
    }
    found
}

pub struct ForeignCwd {
    pub count: usize,
    pub restored: String,
}

/// Directories a transcript works in that no rewrite into `target_cwd` can
/// satisfy, keyed by their path on the *origin* machine. Takes the transcript
/// before rewriting, the only place the origin path still exists losslessly.
pub fn foreign_cwds(raw: &str, spec: &RewriteSpec) -> Vec<(String, ForeignCwd)> {
    transcript_cwds(raw)
        .into_iter()
        .filter_map(|(cwd, count)| {
            let restored = rewrite_transcript(&cwd, spec);
            (restored != spec.target_cwd).then_some((cwd, ForeignCwd { count, restored }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RewriteSpec {
        RewriteSpec {
            origin_cwd: "/Users/mike/code/repo".into(),
            origin_home: "/Users/mike".into(),
            target_cwd: "/home/alice/work/repo".into(),
            target_home: "/home/alice".into(),
            origin_session_id: None,
            target_session_id: None,
        }
    }

    #[test]
    fn nested_paths_rewrite_in_one_pass() {
        let text = "/Users/mike/code/repo/a /Users/mike/.zshrc /Users/mike/code/repo-worktrees/x /Users/mike/projector";
        assert_eq!(
            rewrite_transcript(text, &spec()),
            "/home/alice/work/repo/a /home/alice/.zshrc /home/alice/code/repo-worktrees/x /home/alice/projector"
        );
    }
}
