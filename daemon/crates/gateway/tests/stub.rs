use std::io::{Read, Write};
use std::net::TcpListener;

fn spawn_stub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
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
    });
    format!("http://{}/v1", addr)
}

#[tokio::test]
async fn streams_deltas_and_usage() {
    let base = spawn_stub();
    let cfg = gateway::ProviderConfig { base_url: base, model: "stub".into(), api_key: None };
    let mut seen = String::new();
    let r = gateway::chat_stream(&cfg,
        &[gateway::ChatMessage { role: "user".into(), content: "hi".into() }],
        |d| seen.push_str(d)).await.unwrap();
    assert_eq!(r.content, "Hello stub");
    assert_eq!(seen, "Hello stub");
    assert_eq!(r.usage.unwrap().completion_tokens, 2);
}
