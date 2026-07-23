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
    #[serde(default = "default_kind")]
    pub kind: String, // "orchestrator" | "subagent"
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default = "default_role")]
    pub role: String, // key into config [roles.*]
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub worktree: Option<PathBuf>,
}

fn default_kind() -> String {
    "orchestrator".to_string()
}
fn default_role() -> String {
    "orchestrator".to_string()
}

impl SessionMeta {
    /// The path executors are jailed to: the worktree for subagents,
    /// the workspace for orchestrator sessions.
    pub fn jail_root(&self) -> &Path {
        self.worktree.as_deref().unwrap_or(&self.workspace)
    }

    /// An orchestrator-kind meta for a bare workspace (M0/M1 call sites).
    pub fn orchestrator(workspace: PathBuf) -> Self {
        Self {
            workspace,
            kind: default_kind(),
            parent: None,
            role: default_role(),
            title: None,
            worktree: None,
        }
    }
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

    #[test]
    fn meta_v2_defaults_and_jail_root() {
        // An M1-era meta.json (workspace only) must still load.
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("sessions").join("S1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), br#"{"workspace":"/tmp/ws"}"#).unwrap();
        let m = load_meta(d.path(), "S1").unwrap().unwrap();
        assert_eq!(m.kind, "orchestrator");
        assert_eq!(m.role, "orchestrator");
        assert!(m.parent.is_none());
        assert_eq!(m.jail_root(), Path::new("/tmp/ws"));

        // A subagent meta round-trips and jails to the worktree.
        let sub = SessionMeta {
            workspace: "/tmp/ws".into(),
            kind: "subagent".into(),
            parent: Some("S1".into()),
            role: "coder".into(),
            title: Some("child-a".into()),
            worktree: Some("/tmp/wt".into()),
        };
        write_meta(d.path(), "S2", &sub).unwrap();
        let m2 = load_meta(d.path(), "S2").unwrap().unwrap();
        assert_eq!(m2.kind, "subagent");
        assert_eq!(m2.parent.as_deref(), Some("S1"));
        assert_eq!(m2.jail_root(), Path::new("/tmp/wt"));
    }
}
