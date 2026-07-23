//! CLI ground-truth readback: `composerd sessions` and `composerd ledger <id>`.

use std::process::Command;
use std::path::PathBuf;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_composerd"))
}

#[test]
fn sessions_and_ledger_readback() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().to_path_buf();
    let root = state.join("sessions");
    let store = ledger::SessionStore::new(root.clone(), ledger::Redactor::default());

    let id = store.create_session().unwrap();
    store
        .append(
            &id,
            "human",
            "message",
            "trusted",
            serde_json::json!({"text":"hello"}),
        )
        .unwrap();
    store
        .append(
            &id,
            "orchestrator",
            "message",
            "trusted",
            serde_json::json!({"text":"hi back"}),
        )
        .unwrap();

    // `composerd sessions` lists the id.
    let out = Command::new(bin())
        .args(["sessions"])
        .env("FORGE_COMPOSER_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(out.status.success(), "sessions exited non-zero");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.lines().any(|l| l.trim() == id), "sessions must list id");

    // `composerd ledger <id>` streams the raw ledger lines; parses as the same 2 events.
    let out = Command::new(bin())
        .args(["ledger", &id])
        .env("FORGE_COMPOSER_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(out.status.success(), "ledger exited non-zero");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[1]["seq"], 2);
    assert_eq!(events[0]["body"]["text"], "hello");
    assert_eq!(events[1]["body"]["text"], "hi back");

    // Unknown session -> exit code 2.
    let out = Command::new(bin())
        .args(["ledger", "01JUNKJUNKJUNKJUNKJUNKJUNK"])
        .env("FORGE_COMPOSER_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(!out.status.success(), "unknown session must exit non-zero");
    assert_eq!(out.status.code(), Some(2), "unknown session must exit code 2");
}

#[test]
fn checkpoints_readback() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    let sessions_root = state.join("sessions");
    let store = ledger::SessionStore::new(sessions_root.clone(), ledger::Redactor::default());
    let id = store.create_session().unwrap();
    composerd::state::write_meta(
        &state,
        &id,
        &composerd::state::SessionMeta::orchestrator(ws.clone()),
    )
    .unwrap();
    // Drive Shadow directly to create one checkpoint.
    let sh = tools::Shadow::init(&store.dir(&id), &ws).unwrap();
    let hash = sh.checkpoint("turn-1").unwrap();

    let out = Command::new(bin())
        .args(["checkpoints", &id])
        .env("FORGE_COMPOSER_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "checkpoints exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().any(|l| l.starts_with(&hash)),
        "checkpoint hash not in output: {stdout}"
    );

    // Unknown session -> exit code 2.
    let out = Command::new(bin())
        .args(["checkpoints", "01JUNKJUNKJUNKJUNKJUNKJUNK"])
        .env("FORGE_COMPOSER_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(!out.status.success(), "unknown session must exit non-zero");
    assert_eq!(out.status.code(), Some(2), "unknown session must exit code 2");
}
