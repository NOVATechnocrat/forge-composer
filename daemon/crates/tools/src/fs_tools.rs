//! Filesystem tool executors — all confined to a `Jail`.

use crate::Jail;

pub fn read_file(jail: &Jail, path: &str, max_bytes: usize) -> anyhow::Result<String> {
    let p = jail.resolve(path)?;
    let bytes = std::fs::read(&p)?;
    if bytes.len() <= max_bytes {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        let mut s = String::from_utf8_lossy(&bytes[..max_bytes]).into_owned();
        s.push_str("\n[truncated]");
        Ok(s)
    }
}

pub fn list_dir(jail: &Jail, path: &str) -> anyhow::Result<String> {
    let p = jail.resolve(path)?;
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&p)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false);
        if is_dir {
            names.push(format!("{name}/"));
        } else {
            names.push(name);
        }
    }
    names.sort();
    Ok(names.join("\n"))
}

pub fn search(jail: &Jail, pattern: &str, glob: Option<&str>, max_lines: usize) -> anyhow::Result<String> {
    let root = jail.root();
    let mut cmd = std::process::Command::new("rg");
    cmd.current_dir(root).args(["-n", "--no-heading", "-e", pattern]);
    if let Some(g) = glob {
        cmd.args(["--glob", g]);
    }
    cmd.arg(".");
    let out = cmd.output()?;
    if !out.status.success() {
        // rg exit 1 = no matches.
        return Ok("no matches".to_string());
    }
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        Ok(s)
    } else {
        let mut t = lines[..max_lines].join("\n");
        t.push_str("\n[truncated]");
        Ok(t)
    }
}

pub struct EditOutcome {
    pub path: String,
    pub before: String,
    pub after: String,
}

/// `old_string` empty => full write (create parents). Otherwise replace the
/// FIRST occurrence; zero occurrences is an `Err`. Returns the relative path
/// (relative to the jail root) plus both contents.
pub fn edit_file(jail: &Jail, path: &str, old_string: &str, new_string: &str) -> anyhow::Result<EditOutcome> {
    let full = jail.resolve(path)?;
    let before = if full.exists() {
        std::fs::read_to_string(&full).unwrap_or_default()
    } else {
        String::new()
    };
    let after = if old_string.is_empty() {
        new_string.to_string()
    } else if !before.contains(old_string) {
        anyhow::bail!("old_string not found");
    } else {
        before.replacen(old_string, new_string, 1)
    };
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, &after)?;
    let rel = full
        .strip_prefix(jail.root())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string());
    Ok(EditOutcome {
        path: rel,
        before,
        after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jail_blocks_escape_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let jail = crate::Jail::new(d.path()).unwrap();
        assert!(jail.resolve("../etc/passwd").is_err());
        assert!(jail.resolve("/etc/passwd").is_err());
        std::os::unix::fs::symlink(outside.path(), d.path().join("sneaky")).unwrap();
        assert!(jail.resolve("sneaky/x.txt").is_err());
        assert!(jail.resolve("ok/new-file.txt").is_ok()); // non-existent leaf inside is fine
    }

    #[test]
    fn edit_file_replace_and_full_write() {
        let d = tempfile::tempdir().unwrap();
        let jail = crate::Jail::new(d.path()).unwrap();
        let o = edit_file(&jail, "notes.txt", "", "alpha\n").unwrap();
        assert_eq!(o.before, ""); assert_eq!(o.after, "alpha\n");
        let o = edit_file(&jail, "notes.txt", "alpha", "bravo").unwrap();
        assert_eq!(o.after, "bravo\n");
        assert!(edit_file(&jail, "notes.txt", "missing", "x").is_err());
        assert_eq!(read_file(&jail, "notes.txt", 1024).unwrap(), "bravo\n");
    }

    #[test]
    fn read_caps_and_listing() {
        let d = tempfile::tempdir().unwrap();
        let jail = crate::Jail::new(d.path()).unwrap();
        std::fs::write(d.path().join("big.txt"), "x".repeat(100)).unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let s = read_file(&jail, "big.txt", 10).unwrap();
        assert!(s.starts_with("xxxxxxxxxx") && s.contains("[truncated"));
        let l = list_dir(&jail, ".").unwrap();
        assert!(l.contains("big.txt") && l.contains("sub/"));
    }
}
