//! Where Claude Code keeps its state, as in `loch/claude.py`.

use std::path::{Path, PathBuf};

/// `Path.home()`: `HOME` on Unix, `USERPROFILE` on Windows.
pub fn home_dir() -> String {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(name).unwrap_or_default()
}

/// `CLAUDE_CONFIG_DIR` when set, which is how one machine runs several
/// accounts, else `~/.claude`. Only the variable is consulted, never both
/// places, because that is the rule Claude itself follows — a restore filed
/// anywhere else is one `--resume` can't find. An explicit `home` names
/// another machine's layout and ignores it.
#[allow(dead_code)] // read by list/save/restore/ask once they are ported
pub fn config_dir(home: Option<&str>) -> PathBuf {
    if home.is_none() {
        if let Some(configured) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            return PathBuf::from(configured);
        }
    }
    let home = home.map(str::to_string).unwrap_or_else(home_dir);
    Path::new(&home).join(".claude")
}

#[allow(dead_code)]
pub fn projects_dir(home: Option<&str>) -> PathBuf {
    config_dir(home).join("projects")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_home_ignores_the_variable() {
        assert_eq!(
            projects_dir(Some("/home/other")),
            PathBuf::from("/home/other/.claude/projects")
        );
    }
}
