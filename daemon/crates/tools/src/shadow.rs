//! Shadow-git checkpoints — a git repo whose `.git` lives OUTSIDE the worktree.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Shadow {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl Shadow {
    /// `git init` (idempotent) with `--git-dir=<state_session_dir>/shadow.git`,
    /// work-tree = workspace; sets local user.name/email "forge-composer";
    /// writes `shadow.git/info/exclude` with: `.git/ node_modules/ target/ .venv/`.
    pub fn init(state_session_dir: &Path, work_tree: &Path) -> anyhow::Result<Self> {
        let git_dir = state_session_dir.join("shadow.git");
        git_run(&git_dir, work_tree, &["init", "-q"])?;
        git_run(&git_dir, work_tree, &["config", "user.name", "forge-composer"])?;
        git_run(&git_dir, work_tree, &["config", "user.email", "forge-composer@local"])?;
        let exclude = git_dir.join("info").join("exclude");
        if let Some(parent) = exclude.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&exclude, ".git/\nnode_modules/\ntarget/\n.venv/\n")?;
        Ok(Self {
            git_dir,
            work_tree: work_tree.to_path_buf(),
        })
    }

    /// `add -A`; `commit --allow-empty -m label`; returns the commit hash.
    pub fn checkpoint(&self, label: &str) -> anyhow::Result<String> {
        git_run(&self.git_dir, &self.work_tree, &["add", "-A"])?;
        git_run(
            &self.git_dir,
            &self.work_tree,
            &["commit", "-q", "--allow-empty", "-m", label],
        )?;
        let hash = git_out(&self.git_dir, &self.work_tree, &["rev-parse", "HEAD"])?;
        Ok(hash.trim().to_string())
    }

    /// `(hash, label)`, newest first; empty repo => `Ok(vec![])`.
    pub fn list(&self) -> anyhow::Result<Vec<(String, String)>> {
        let out = git_out(&self.git_dir, &self.work_tree, &["log", "--format=%H%x09%s"]);
        match out {
            Ok(s) => {
                let mut rows = Vec::new();
                for line in s.lines() {
                    if let Some((h, label)) = line.split_once('\t') {
                        rows.push((h.to_string(), label.to_string()));
                    }
                }
                Ok(rows)
            }
            Err(_) => Ok(Vec::new()),
        }
    }

    /// `checkout <hash> -- .` (work-tree files only).
    pub fn restore(&self, hash: &str) -> anyhow::Result<()> {
        git_run(&self.git_dir, &self.work_tree, &["checkout", "-q", hash, "--", "."])
    }

    /// `git show hash:rel_path`.
    pub fn file_at(&self, hash: &str, rel_path: &str) -> anyhow::Result<String> {
        let spec = format!("{hash}:{rel_path}");
        git_out(&self.git_dir, &self.work_tree, &["show", &spec])
    }
}

fn git_run(git_dir: &Path, work_tree: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(["--git-dir"])
        .arg(git_dir)
        .args(["--work-tree"])
        .arg(work_tree)
        .args(args)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {err}", args.join(" "));
    }
    Ok(())
}

fn git_out(git_dir: &Path, work_tree: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(["--git-dir"])
        .arg(git_dir)
        .args(["--work-tree"])
        .arg(work_tree)
        .args(args)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {err}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_restore_and_file_at() {
        let state = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.txt"), "one\n").unwrap();
        let sh = Shadow::init(state.path(), ws.path()).unwrap();
        let h1 = sh.checkpoint("turn-1").unwrap();
        std::fs::write(ws.path().join("a.txt"), "two\n").unwrap();
        let _h2 = sh.checkpoint("turn-2").unwrap();
        assert_eq!(sh.list().unwrap().len(), 2);
        assert_eq!(sh.file_at(&h1, "a.txt").unwrap(), "one\n");
        sh.restore(&h1).unwrap();
        assert_eq!(std::fs::read_to_string(ws.path().join("a.txt")).unwrap(), "one\n");
    }
}
