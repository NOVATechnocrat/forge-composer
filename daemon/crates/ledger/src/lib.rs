//! ledger — append-only JSONL event store for Forge Composer sessions.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub const SCHEMA: &str = "forgeloop.composer.event.v1";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Event {
    pub v: String,
    pub seq: u64,
    pub ts: String,
    pub session: String,
    pub actor: String,
    pub kind: String,
    pub provenance: String,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// Drops empty strings; non-empty secrets are kept in the order given.
    pub fn new(secrets: Vec<String>) -> Self {
        Self {
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }

    /// Replace every occurrence of each known secret with "[REDACTED]".
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            if s.is_empty() {
                continue;
            }
            if out.contains(s) {
                out = out.replace(s, "[REDACTED]");
            }
        }
        out
    }
}

pub struct SessionStore {
    root: PathBuf,
    redactor: Redactor,
    seq_lock: Mutex<()>,
}

impl SessionStore {
    pub fn new(root: std::path::PathBuf, redactor: Redactor) -> Self {
        Self {
            root,
            redactor,
            seq_lock: Mutex::new(()),
        }
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.root.join(session)
    }

    /// Absolute path of a session's directory (the dir holding `ledger.jsonl`).
    pub fn dir(&self, session: &str) -> PathBuf {
        self.session_dir(session)
    }

    fn ledger_path(&self, session: &str) -> PathBuf {
        self.session_dir(session).join("ledger.jsonl")
    }

    pub fn create_session(&self) -> anyhow::Result<String> {
        std::fs::create_dir_all(&self.root)?;
        let id = ulid::Ulid::new().to_string();
        let dir = self.session_dir(&id);
        std::fs::create_dir_all(&dir)?;
        // Empty ledger so list/events work before the first append (extension
        // init fetches /events immediately after createSession).
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(self.ledger_path(&id))?;
        Ok(id)
    }

    pub fn list_sessions(&self) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                // A session directory is enough — create_session always leaves a
                // ledger.jsonl, but tolerate older dirs that only have meta.json.
                if self.session_exists(name) {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// True if a session directory exists (regardless of whether it has events yet).
    pub fn session_exists(&self, session: &str) -> bool {
        self.session_dir(session).exists()
    }

    fn last_seq(&self, session: &str) -> anyhow::Result<u64> {
        let path = self.ledger_path(session);
        if !path.exists() {
            return Ok(0);
        }
        let mut max = 0u64;
        let bytes = std::fs::read(&path)?;
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let ev: Event = serde_json::from_slice(line)
                .map_err(|e| anyhow::anyhow!("corrupt ledger line: {e}"))?;
            if ev.seq > max {
                max = ev.seq;
            }
        }
        Ok(max)
    }

    pub fn append(
        &self,
        session: &str,
        actor: &str,
        kind: &str,
        provenance: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<Event> {
        let _guard = self.seq_lock.lock().unwrap();
        let dir = self.session_dir(session);
        if !dir.exists() {
            anyhow::bail!("unknown session: {session}");
        }
        let seq = self.last_seq(session)? + 1;
        let ts = jiff::Timestamp::now().to_string();
        let event = Event {
            v: SCHEMA.to_string(),
            seq,
            ts,
            session: session.to_string(),
            actor: actor.to_string(),
            kind: kind.to_string(),
            provenance: provenance.to_string(),
            body,
        };
        let mut raw = serde_json::to_string(&event)?;
        raw = self.redactor.scrub(&raw);
        let path = self.ledger_path(session);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(raw.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(event)
    }

    pub fn read(&self, session: &str, since_seq: u64) -> anyhow::Result<Vec<Event>> {
        if !self.session_exists(session) {
            anyhow::bail!("unknown session: {session}");
        }
        let path = self.ledger_path(session);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = std::fs::read(&path)?;
        let mut out = Vec::new();
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let ev: Event = serde_json::from_slice(line)
                .map_err(|e| anyhow::anyhow!("corrupt ledger line: {e}"))?;
            if ev.seq > since_seq {
                out.push(ev);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> (tempfile::TempDir, SessionStore) {
        let d = tempfile::tempdir().unwrap();
        let s = SessionStore::new(d.path().join("sessions"), Redactor::default());
        (d, s)
    }

    #[test]
    fn append_assigns_dense_seq_and_schema() {
        let (_d, s) = store();
        let id = s.create_session().unwrap();
        let e1 = s.append(&id, "human", "message", "trusted", serde_json::json!({"text":"hi"})).unwrap();
        let e2 = s.append(&id, "orchestrator", "message", "trusted", serde_json::json!({"text":"yo"})).unwrap();
        assert_eq!((e1.seq, e2.seq), (1, 2));
        assert_eq!(e1.v, SCHEMA);
        assert_eq!(e1.session, id);
    }

    #[test]
    fn read_since_and_persistence_across_reopen() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("sessions");
        let id = {
            let s = SessionStore::new(root.clone(), Redactor::default());
            let id = s.create_session().unwrap();
            for i in 0..3 {
                s.append(&id, "human", "message", "trusted", serde_json::json!({"i": i})).unwrap();
            }
            id
        };
        let s2 = SessionStore::new(root, Redactor::default());
        let all = s2.read(&id, 0).unwrap();
        let tail = s2.read(&id, 2).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 3);
        assert_eq!(s2.append(&id, "human", "message", "trusted", serde_json::json!({})).unwrap().seq, 4);
    }

    #[test]
    fn redactor_scrubs_secrets_from_persisted_bytes() {
        let d = tempfile::tempdir().unwrap();
        let s = SessionStore::new(d.path().join("sessions"),
                                  Redactor::new(vec!["sk-SECRET-123".into()]));
        let id = s.create_session().unwrap();
        s.append(&id, "human", "message", "trusted",
                 serde_json::json!({"text":"my key is sk-SECRET-123 ok"})).unwrap();
        let raw = std::fs::read_to_string(
            d.path().join("sessions").join(&id).join("ledger.jsonl")).unwrap();
        assert!(!raw.contains("sk-SECRET-123"));
        assert!(raw.contains("[REDACTED]"));
    }

    #[test]
    fn unknown_session_read_is_error_not_empty() {
        let (_d, s) = store();
        assert!(s.read("01JUNKJUNKJUNKJUNKJUNKJUNK", 0).is_err());
    }

    #[test]
    fn fresh_session_lists_and_reads_empty() {
        let (_d, s) = store();
        let id = s.create_session().unwrap();
        assert_eq!(s.list_sessions().unwrap(), vec![id.clone()]);
        assert!(s.read(&id, 0).unwrap().is_empty());
        assert!(s.session_dir(&id).join("ledger.jsonl").exists());
    }
}
