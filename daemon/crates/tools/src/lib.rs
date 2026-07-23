//! tools — worktree-jailed executors + shadow-git checkpoints.

pub mod fs_tools;
pub mod shadow;
pub mod terminal;
pub mod worktree;

pub use shadow::Shadow;

use std::path::{Path, PathBuf};

/// A canonicalized workspace root that all tool paths are confined to.
pub struct Jail {
    root: PathBuf,
}

impl Jail {
    /// Canonicalize `root` at construction so symlink/`..` escape checks are
    /// always against the real filesystem location.
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        let root = root
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("jail root canonicalize failed: {e}"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `path` (absolute allowed if inside) to a real filesystem path
    /// that must remain within the jail. Symlinks are resolved before the
    /// check; a non-existent leaf inside the jail is permitted (its existing
    /// ancestor is canonicalized and the tail re-appended).
    pub fn resolve(&self, path: &str) -> anyhow::Result<PathBuf> {
        let p = Path::new(path);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        // Walk to the deepest existing ancestor, canonicalize it, re-append tail.
        let mut ancestor = joined.clone();
        let mut tail: Vec<String> = Vec::new();
        while !ancestor.exists() {
            let file_name = ancestor
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("path escapes workspace jail"))?
                .to_string_lossy()
                .to_string();
            tail.push(file_name);
            ancestor = ancestor
                .parent()
                .ok_or_else(|| anyhow::anyhow!("path escapes workspace jail"))?
                .to_path_buf();
        }
        let canon = ancestor
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("canonicalize failed: {e}"))?;
        if !canon.starts_with(&self.root) {
            return Err(anyhow::anyhow!("path escapes workspace jail"));
        }
        let mut full = canon;
        for part in tail.into_iter().rev() {
            full.push(part);
        }
        if !full.starts_with(&self.root) {
            return Err(anyhow::anyhow!("path escapes workspace jail"));
        }
        Ok(full)
    }
}
