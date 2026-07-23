//! State-directory resolution, auth token, and daemon.json.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// `$FORGE_COMPOSER_STATE_DIR` if set, else `~/.local/share/forge-composer`.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FORGE_COMPOSER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".local/share/forge-composer")
}

/// Read an existing 0600 auth token, or create a fresh 64-hex-char one.
/// Idempotent: a second call returns the identical token.
pub fn ensure_auth_token(dir: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("auth.token");
    if path.exists() {
        let token = std::fs::read_to_string(&path)?;
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = random_hex_token();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&path, perms)?;
    Ok(token)
}

fn random_hex_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Write `{"port":N,"pid":N}` to `<dir>/daemon.json`.
pub fn write_daemon_json(dir: &Path, port: u16) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("daemon.json");
    let body = serde_json::json!({"port": port, "pid": std::process::id()});
    let bytes = serde_json::to_vec(&body)?;
    std::fs::write(&path, bytes)?;
    Ok(())
}

/// Per-session metadata persisted at `<state_dir>/sessions/<id>/meta.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub workspace: PathBuf,
}

/// Write a session's `meta.json` (idempotent overwrite).
pub fn write_meta(state_dir: &Path, session: &str, meta: &SessionMeta) -> anyhow::Result<()> {
    let dir = state_dir.join("sessions").join(session);
    std::fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec(meta)?;
    std::fs::write(dir.join("meta.json"), bytes)?;
    Ok(())
}

/// Load a session's `meta.json`. Returns `Ok(None)` if the session dir exists
/// but has no meta.json (e.g. an M0-era session).
pub fn load_meta(state_dir: &Path, session: &str) -> anyhow::Result<Option<SessionMeta>> {
    let path = state_dir.join("sessions").join(session).join("meta.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    let meta: SessionMeta = serde_json::from_slice(&bytes)?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_auth_token_creates_0600_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let t1 = ensure_auth_token(d.path()).unwrap();
        assert_eq!(t1.len(), 64);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        let meta = std::fs::metadata(d.path().join("auth.token")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let t2 = ensure_auth_token(d.path()).unwrap();
        assert_eq!(t1, t2);
    }

    #[test]
    fn state_dir_respects_env() {
        std::env::set_var("FORGE_COMPOSER_STATE_DIR", "/tmp/fc-state-test-xyz");
        assert_eq!(state_dir(), PathBuf::from("/tmp/fc-state-test-xyz"));
        std::env::remove_var("FORGE_COMPOSER_STATE_DIR");
    }

    #[test]
    fn write_daemon_json_round_trips() {
        let d = tempfile::tempdir().unwrap();
        write_daemon_json(d.path(), 8642).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("daemon.json")).unwrap())
                .unwrap();
        assert_eq!(v["port"], 8642);
        assert!(v["pid"].as_u64().is_some());
    }
}
