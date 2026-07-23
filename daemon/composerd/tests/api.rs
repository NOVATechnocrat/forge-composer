//! M0 spine integration test: auth, session create, orchestrator turn via a
//! raw-TCP OpenAI-compatible stub, usage capture, and reattach persistence.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use composerd::testkit;

fn spawn_stub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    static STARTED: AtomicBool = AtomicBool::new(false);
    let _ = STARTED.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        for _ in 0..16 {
            let (mut s, _) = match l.accept() {
                Ok(p) => p,
                Err(_) => return,
            };
            let mut buf = [0u8; 65536];
            let _ = s.read(&mut buf);
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"stub\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n");
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body);
            let _ = s.write_all(resp.as_bytes());
        }
    });
    format!("http://{}/v1", addr)
}

async fn boot(state_dir: &std::path::Path) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, String) {
    std::env::set_var("FORGE_COMPOSER_STATE_DIR", state_dir);
    let (addr, handle) = testkit::serve().await;
    let token = std::fs::read_to_string(state_dir.join("auth.token")).unwrap();
    (addr, handle, token.trim().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_session_turn_and_reattach() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().to_path_buf();

    let base = spawn_stub();
    // Write a config pointing orchestrator at the stub.
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(
        state_dir.join("config.toml"),
        format!(
            r#"[server]
port = 0

[providers.stub]
base_url = "{base}"

[roles.orchestrator]
provider = "stub"
model = "stub-model"
"#,
            base = base
        ),
    )
    .unwrap();

    let (addr, handle, token) = boot(&state_dir).await;
    let client = reqwest::Client::new();

    // Auth: no token -> 401, with token -> 200.
    let no_auth = client
        .get(format!("http://{addr}/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), 401);

    let bearer = format!("Bearer {token}");

    // Create a session.
    let create = client
        .post(format!("http://{addr}/sessions"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 200);
    let cid: serde_json::Value = create.json().await.unwrap();
    let id = cid["id"].as_str().unwrap().to_string();

    // Send a message.
    let msg = client
        .post(format!("http://{addr}/sessions/{id}/message"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"text":"ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(msg.status(), 200);

    // Poll events until an orchestrator message containing "Hello stub" appears (5s budget).
    let mut found_orch = false;
    let mut usage_tokens = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let evs = client
            .get(format!("http://{addr}/sessions/{id}/events?since=0"))
            .header("Authorization", &bearer)
            .send()
            .await
            .unwrap();
        if evs.status() != 200 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let v: serde_json::Value = evs.json().await.unwrap();
        for ev in v["events"].as_array().unwrap() {
            if ev["kind"] == "message" && ev["actor"] == "orchestrator" {
                if ev["body"]["text"].as_str().unwrap_or("").contains("Hello stub") {
                    found_orch = true;
                }
            }
            if ev["kind"] == "usage" {
                usage_tokens = Some(ev["body"]["completion_tokens"].as_u64().unwrap_or(0));
            }
        }
        if found_orch {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(found_orch, "orchestrator reply never appeared");
    assert_eq!(usage_tokens, Some(2), "usage completion_tokens must be 2");

    // Kill the daemon and re-serve on the same state dir; events must persist.
    handle.abort();
    // Give the abort a moment to release the port.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (addr2, handle2, token2) = boot(&state_dir).await;
    assert_eq!(token, token2, "auth token must be idempotent across restarts");
    let evs = client
        .get(format!("http://{addr2}/sessions/{id}/events?since=0"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    let v: serde_json::Value = evs.json().await.unwrap();
    let count = v["events"].as_array().unwrap().len();
    assert!(count >= 3, "prior events must survive reattach; got {count}");

    handle2.abort();
    let _ = Arc::new(()); // keep `Arc` import used
}
