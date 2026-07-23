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
    let cfg = gateway::ProviderConfig {
        base_url: base,
        model: "stub".into(),
        api_key: None,
        kind: gateway::ProviderKind::OpenAI,
    };
    let mut seen = String::new();
    let r = gateway::chat_stream(
        &cfg,
        &[gateway::ChatMessage::text("user", "hi")],
        None,
        |d| seen.push_str(d),
    )
    .await
    .unwrap();
    assert_eq!(r.content, "Hello stub");
    assert_eq!(seen, "Hello stub");
    assert_eq!(r.usage.unwrap().completion_tokens, 2);
    assert!(r.tool_calls.is_empty());
}

fn spawn_tool_stub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 65536];
        let _ = s.read(&mut buf);
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"terminal\",\"arguments\":\"{\\\"comm\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n");
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(), body);
        let _ = s.write_all(resp.as_bytes());
    });
    format!("http://{}/v1", addr)
}

#[tokio::test]
async fn parses_streamed_tool_calls() {
    let base = spawn_tool_stub();
    let cfg = gateway::ProviderConfig {
        base_url: base,
        model: "stub".into(),
        api_key: None,
        kind: gateway::ProviderKind::OpenAI,
    };
    let r = gateway::chat_stream(
        &cfg,
        &[gateway::ChatMessage::text("user", "do it")],
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        r.tool_calls,
        vec![gateway::ToolCall {
            id: "call_1".into(),
            name: "terminal".into(),
            arguments: "{\"command\":\"ls\"}".into(),
        }]
    );
    assert!(r.content.is_empty());
}
