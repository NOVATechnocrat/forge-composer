use std::io::{Read, Write};
use std::net::TcpListener;

fn spawn_anthropic_stub() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 65536];
        let _ = s.read(&mut buf);
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi \"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\".txt\\\"}\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{},\"usage\":{\"output_tokens\":3}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(resp.as_bytes());
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn anthropic_streams_text_and_tool_use() {
    let base = spawn_anthropic_stub();
    let cfg = gateway::ProviderConfig {
        base_url: base,
        model: "claude-stub".into(),
        api_key: Some("sk-anthropic".into()),
        kind: gateway::ProviderKind::Anthropic,
    };
    let mut seen = String::new();
    let r = gateway::chat(
        &cfg,
        &[
            gateway::ChatMessage::text("system", "be brief"),
            gateway::ChatMessage::text("user", "read a.txt"),
        ],
        None,
        |d| seen.push_str(d),
    )
    .await
    .unwrap();
    assert_eq!(r.content, "Hi ");
    assert_eq!(seen, "Hi ");
    assert_eq!(
        r.tool_calls,
        vec![gateway::ToolCall {
            id: "tu_1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"a.txt\"}".into(),
        }]
    );
    let u = r.usage.unwrap();
    assert_eq!((u.prompt_tokens, u.completion_tokens), (7, 3));
}
