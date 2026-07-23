//! Git worktree lifecycle for subagent isolation (design D5).

use std::path::Path;
use std::process::Command;

fn run(repo: &Path, args: &[&str], label: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("{label}: spawn git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{label} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `git -C <repo> worktree add -b <branch> <dest>`.
pub fn add(repo: &Path, dest: &Path, branch: &str) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    run(
        repo,
        &["worktree", "add", "-b", branch, &dest.to_string_lossy()],
        "git worktree add",
    )
}

/// `git -C <repo> worktree remove --force <dest>`.
pub fn remove(repo: &Path, dest: &Path) -> anyhow::Result<()> {
    run(
        repo,
        &["worktree", "remove", "--force", &dest.to_string_lossy()],
        "git worktree remove",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    #[test]
    fn add_creates_checked_out_worktree_and_remove_deletes_it() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);

        let dest = d.path().join("wt");
        add(&repo, &dest, "fc/test-branch").unwrap();
        assert!(dest.join("f.txt").exists(), "worktree file checked out");
        let head = std::process::Command::new("git")
            .args(["-C", dest.to_str().unwrap(), "rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "fc/test-branch");

        remove(&repo, &dest).unwrap();
        assert!(!dest.exists(), "worktree removed");
    }

    #[test]
    fn add_fails_cleanly_outside_a_git_repo() {
        let d = tempfile::tempdir().unwrap();
        let err = add(d.path(), &d.path().join("wt"), "fc/x").unwrap_err();
        assert!(err.to_string().contains("git worktree add"), "{err}");
    }
}
