use loch::cli::current_branch;
use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loch-test-git-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn current_branch_reads_the_checkout() {
    let tmp = scratch("reads");
    let repo = tmp.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "-b", "feature/foo"])
        .arg(&repo)
        .output()
        .unwrap()
        .status;
    assert!(status.success());

    assert_eq!(
        current_branch(repo.to_str().unwrap()).as_deref(),
        Some("feature/foo")
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

// None covers both a detached HEAD and a directory outside any repo — the
// same two cases that leave gitBranch unset on a session.
#[test]
fn current_branch_is_none_outside_a_repo() {
    let tmp = scratch("outside");

    assert_eq!(current_branch(tmp.to_str().unwrap()), None);
    let _ = std::fs::remove_dir_all(&tmp);
}
