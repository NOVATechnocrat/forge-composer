//! M0 spine integration test: auth, session create, orchestrator turn via a
//! raw-TCP OpenAI-compatible stub, usage capture, and reattach persistence.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use composerd::testkit;

// Tests in this file drive the daemon via a process-global state-dir env var,
// so serialize them to avoid one test clobbering another's FORGE_COMPOSER_STATE_DIR.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn spawn_stub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
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
    let _g = TEST_LOCK.lock().unwrap();
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
}

// ===== M1 agentic tool-loop integration tests =====
//
// A scriptable raw-TCP OpenAI stub: responds to a conversation containing a
// marker. With no tool message yet it emits a tool_call; once a tool message is
// present it emits a final text reply.

fn spawn_scripted_stub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..64 {
            let (mut s, _) = match l.accept() {
                Ok(p) => p,
                Err(_) => return,
            };
            s.set_read_timeout(Some(std::time::Duration::from_millis(500))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            // Read until we have the full body (Content-Length) or the peer stops.
            loop {
                match s.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if has_full_body(&buf) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&buf).to_string();
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
            let messages = parsed.get("messages").and_then(|m| m.as_array());
            let convo = messages
                .map(|ms| {
                    ms.iter()
                        .map(|m| {
                            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                                c.to_string()
                            } else {
                                serde_json::to_string(&m.get("content")).unwrap_or_default()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let has_tool = messages
                .map(|ms| ms.iter().any(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool")))
                .unwrap_or(false);
            let last_tool = messages
                .map(|ms| {
                    ms.iter()
                        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
                        .last()
                        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();

            let payload = if convo.contains("T6-RM") {
                if has_tool {
                    text_sse("T6-DONE-RM")
                } else {
                    tool_sse("terminal", r#"{"command":"rm -rf canary-dir"}"#)
                }
            } else if convo.contains("T6-EDIT") {
                if has_tool {
                    text_sse("T6-EDIT-DONE")
                } else {
                    tool_sse("edit_file", r#"{"path":"notes.txt","old_string":"alpha","new_string":"bravo"}"#)
                }
            } else if convo.contains("T6-ECHO") {
                if has_tool {
                    text_sse(&format!("T6-DONE {last_tool}"))
                } else {
                    tool_sse("terminal", r#"{"command":"echo t6-tool-ok"}"#)
                }
            } else {
                text_sse("FORGE-COMPOSER-STUB-REPLY pong")
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    format!("http://{}/v1", addr)
}

// True once `buf` contains the headers plus the full Content-Length body.
fn has_full_body(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    let Some((headers, body)) = s.split_once("\r\n\r\n") else {
        return false;
    };
    let cl = headers
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    body.len() >= cl
}

fn text_sse(text: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text}}]})
    ));
    s.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"choices":[{"delta":{}}],"usage":{"prompt_tokens":7,"completion_tokens":2}})
    ));
    s.push_str("data: [DONE]\n\n");
    s
}

fn tool_sse(name: &str, args_json: &str) -> String {
    let half = (args_json.len() / 2).max(1);
    let (a, b) = (&args_json[..half], &args_json[half..]);
    let mut s = String::new();
    s.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_t6","type":"function","function":{"name":name,"arguments":a}}]}}]})
    ));
    s.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":b}}]}}]})
    ));
    s.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"choices":[{"delta":{}}],"usage":{"prompt_tokens":9,"completion_tokens":4}})
    ));
    s.push_str("data: [DONE]\n\n");
    s
}

async fn boot_with_stub(state_dir: &std::path::Path, base_url: &str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, String) {
    std::env::set_var("FORGE_COMPOSER_STATE_DIR", state_dir);
    std::fs::create_dir_all(state_dir).unwrap();
    std::fs::write(
        state_dir.join("config.toml"),
        format!(
            r#"[server]
port = 0

[policy]
approval_timeout_secs = 30

[providers.stub]
base_url = "{base_url}"

[roles.orchestrator]
provider = "stub"
model = "stub-model"
"#,
            base_url = base_url
        ),
    )
    .unwrap();
    let (addr, handle) = testkit::serve().await;
    let token = std::fs::read_to_string(state_dir.join("auth.token")).unwrap();
    (addr, handle, token.trim().to_string())
}

async fn poll_events(client: &reqwest::Client, base: &str, bearer: &str, id: &str) -> serde_json::Value {
    client
        .get(format!("{base}/sessions/{id}/events?since=0"))
        .header("Authorization", bearer)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_flow_executes_tool_and_records_events() {
    let _g = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state_dir = tmp.path().join("state");
    let base = spawn_scripted_stub();
    let (addr, handle, token) = boot_with_stub(&state_dir, &base).await;
    let client = reqwest::Client::new();
    let bearer = format!("Bearer {token}");

    let id = client
        .post(format!("http://{addr}/sessions"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"workspace": ws}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    client
        .post(format!("http://{addr}/sessions/{id}/message"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"text":"please T6-ECHO"}))
        .send()
        .await
        .unwrap();

    // poll until an approval_request appears
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut req_id = None;
    while std::time::Instant::now() < deadline {
        let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
        if let Some(reqs) = v["events"].as_array().and_then(|e| {
            Some(
                e.iter()
                    .filter(|ev| ev["kind"] == "approval_request")
                    .collect::<Vec<_>>(),
            )
        }) {
            if !reqs.is_empty() {
                req_id = Some(reqs[0]["body"]["id"].as_str().unwrap().to_string());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let req_id = req_id.expect("no approval_request appeared");

    client
        .post(format!("http://{addr}/sessions/{id}/approve"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"id": req_id, "approved": true}))
        .send()
        .await
        .unwrap();

    // poll until orchestrator message "T6-DONE"
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut done = false;
    while std::time::Instant::now() < deadline {
        let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
        if v["events"].as_array().unwrap().iter().any(|e| {
            e["kind"] == "message"
                && e["actor"] == "orchestrator"
                && e["body"]["text"].as_str().unwrap_or("").contains("T6-DONE")
        }) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(done, "T6-DONE never appeared");

    let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
    let evs = v["events"].as_array().unwrap();
    let kinds: Vec<&str> = evs.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"tool_call"), "missing tool_call: {kinds:?}");
    assert!(kinds.contains(&"approval_request"), "missing approval_request");
    assert!(kinds.contains(&"approval_decision"), "missing approval_decision");
    let result = evs
        .iter()
        .filter(|e| e["kind"] == "tool_result")
        .last()
        .expect("no tool_result");
    assert_eq!(result["provenance"], "untrusted");
    assert_eq!(result["body"]["ok"], true);
    assert!(
        result["body"]["output"].as_str().unwrap_or("").contains("t6-tool-ok"),
        "tool output missing: {}",
        result["body"]
    );
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_flow_never_asks_and_never_executes() {
    let _g = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(ws.join("canary-dir")).unwrap();
    std::fs::write(ws.join("canary-dir").join("keep.txt"), "precious").unwrap();
    let state_dir = tmp.path().join("state");
    let base = spawn_scripted_stub();
    let (addr, handle, token) = boot_with_stub(&state_dir, &base).await;
    let client = reqwest::Client::new();
    let bearer = format!("Bearer {token}");

    let id = client
        .post(format!("http://{addr}/sessions"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"workspace": ws}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    client
        .post(format!("http://{addr}/sessions/{id}/message"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"text":"please T6-RM"}))
        .send()
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut done = false;
    while std::time::Instant::now() < deadline {
        let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
        if v["events"].as_array().unwrap().iter().any(|e| {
            e["kind"] == "message"
                && e["actor"] == "orchestrator"
                && e["body"]["text"].as_str().unwrap_or("").contains("T6-DONE-RM")
        }) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(done, "T6-DONE-RM never appeared");

    let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
    let evs = v["events"].as_array().unwrap();
    assert!(
        !evs.iter().any(|e| e["kind"] == "approval_request"),
        "hard deny must not ask for approval"
    );
    let result = evs
        .iter()
        .filter(|e| e["kind"] == "tool_result")
        .last()
        .expect("no tool_result");
    assert_eq!(result["body"]["denied"], true);
    assert!(
        result["body"]["output"].as_str().unwrap_or("").contains("DENIED by policy"),
        "deny reason missing: {}",
        result["body"]
    );
    assert!(ws.join("canary-dir").join("keep.txt").exists(), "canary destroyed");
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_checkpoints_and_restore_round_trip() {
    let _g = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.txt"), "alpha\n").unwrap();
    let state_dir = tmp.path().join("state");
    let base = spawn_scripted_stub();
    let (addr, handle, token) = boot_with_stub(&state_dir, &base).await;
    let client = reqwest::Client::new();
    let bearer = format!("Bearer {token}");

    let id = client
        .post(format!("http://{addr}/sessions"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"workspace": ws}))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    client
        .post(format!("http://{addr}/sessions/{id}/message"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"text":"please T6-EDIT"}))
        .send()
        .await
        .unwrap();

    // approve the edit
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut req_id = None;
    while std::time::Instant::now() < deadline {
        let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
        if let Some(r) = v["events"].as_array().and_then(|e| {
            e.iter()
                .find(|ev| ev["kind"] == "approval_request")
                .map(|ev| ev["body"]["id"].as_str().unwrap().to_string())
        }) {
            req_id = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let req_id = req_id.expect("no approval_request for edit");
    client
        .post(format!("http://{addr}/sessions/{id}/approve"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"id": req_id, "approved": true}))
        .send()
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut done = false;
    while std::time::Instant::now() < deadline {
        let v = poll_events(&client, &format!("http://{addr}"), &bearer, &id).await;
        if v["events"].as_array().unwrap().iter().any(|e| {
            e["kind"] == "message"
                && e["actor"] == "orchestrator"
                && e["body"]["text"].as_str().unwrap_or("").contains("T6-EDIT-DONE")
        }) {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(done, "T6-EDIT-DONE never appeared");
    assert_eq!(std::fs::read_to_string(ws.join("notes.txt")).unwrap(), "bravo\n");

    let ckpts = client
        .get(format!("http://{addr}/sessions/{id}/checkpoints"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let list = ckpts["checkpoints"].as_array().unwrap();
    assert!(!list.is_empty(), "no checkpoints");
    let hash = list[0]["hash"].as_str().unwrap().to_string();

    let file_at = client
        .get(format!("http://{addr}/sessions/{id}/file_at?hash={hash}&path=notes.txt"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(file_at, "alpha\n");

    client
        .post(format!("http://{addr}/sessions/{id}/restore"))
        .header("Authorization", &bearer)
        .json(&serde_json::json!({"hash": hash}))
        .send()
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("notes.txt")).unwrap(), "alpha\n");
    handle.abort();
}
