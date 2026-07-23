//! Anthropic Messages API streaming adapter — `pub(crate)` arm of `chat()`.

use crate::{ChatMessage, ChatResult, ChatUsage, ProviderConfig, ToolCall};

/// POST `{base_url}/v1/messages` (stream:true), parse Anthropic SSE events into
/// the same `ChatResult` shape the OpenAI adapter produces.
pub(crate) async fn chat_anthropic(
    cfg: &ProviderConfig,
    messages: &[ChatMessage],
    tools: Option<&serde_json::Value>,
    mut on_delta: impl FnMut(&str),
) -> anyhow::Result<ChatResult> {
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));

    let system = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());
    let api_messages = build_anthropic_messages(messages);
    let mut body = serde_json::json!({
        "model": cfg.model,
        "max_tokens": 8192,
        "stream": true,
        "messages": api_messages,
    });
    if let Some(s) = system {
        body["system"] = serde_json::Value::String(s);
    }
    if let Some(t) = tools {
        if let Some(converted) = convert_tools(t) {
            body["tools"] = converted;
        }
    }

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| anyhow::anyhow!("http client build failed: {e}"))?;
    let mut req = client.post(&url).json(&body);
    if let Some(key) = &cfg.api_key {
        req = req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("anthropic request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("anthropic messages returned {status}: {text}");
    }

    let mut stream = resp.bytes_stream();
    use futures::StreamExt;

    let mut buf = Vec::<u8>::new();
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut prompt_tokens: u64 = 0;
    let mut completion_tokens: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("stream read failed: {e}"))?;
        buf.extend_from_slice(&chunk);
        loop {
            let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") else {
                break;
            };
            let frame: Vec<u8> = buf.drain(..idx + 2).collect();
            let frame_str = String::from_utf8_lossy(&frame[..frame.len() - 2]);
            for line in frame_str.split('\n') {
                let line = line.strip_prefix('\r').unwrap_or(line);
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let v: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let ev_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ev_type {
                    "message_start" => {
                        if let Some(it) = v
                            .get("message")
                            .and_then(|m| m.get("usage"))
                            .and_then(|u| u.get("input_tokens"))
                            .and_then(|t| t.as_u64())
                        {
                            prompt_tokens = it;
                        }
                    }
                    "content_block_start" => {
                        if let Some(block) = v.get("content_block") {
                            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                let id = block
                                    .get("id")
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                tool_calls.push(ToolCall {
                                    id,
                                    name,
                                    arguments: String::new(),
                                });
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let Some(delta) = v.get("delta") {
                            match delta.get("type").and_then(|t| t.as_str()) {
                                Some("text_delta") => {
                                    if let Some(text) =
                                        delta.get("text").and_then(|t| t.as_str())
                                    {
                                        content.push_str(text);
                                        on_delta(text);
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(pj) =
                                        delta.get("partial_json").and_then(|t| t.as_str())
                                    {
                                        if let Some(last) = tool_calls.last_mut() {
                                            last.arguments.push_str(pj);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(ot) = v
                            .get("usage")
                            .and_then(|u| u.get("output_tokens"))
                            .and_then(|t| t.as_u64())
                        {
                            completion_tokens = ot;
                        }
                    }
                    "message_stop" => {
                        return Ok(ChatResult {
                            content,
                            usage: Some(ChatUsage {
                                prompt_tokens,
                                completion_tokens,
                            }),
                            tool_calls,
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(ChatResult {
        content,
        usage: Some(ChatUsage {
            prompt_tokens,
            completion_tokens,
        }),
        tool_calls,
    })
}

fn build_anthropic_messages(messages: &[ChatMessage]) -> serde_json::Value {
    let arr = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| {
            if m.role == "tool" {
                serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                        "content": m.content,
                    }]
                })
            } else if let Some(calls) = &m.tool_calls {
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({"type":"text","text": m.content}));
                }
                for c in calls {
                    let input: serde_json::Value =
                        serde_json::from_str(&c.arguments).unwrap_or(serde_json::json!({}));
                    blocks.push(serde_json::json!({
                        "type":"tool_use","id": c.id,"name": c.name,"input": input
                    }));
                }
                serde_json::json!({"role":"assistant","content": blocks})
            } else {
                serde_json::json!({"role": m.role, "content": m.content})
            }
        })
        .collect();
    serde_json::Value::Array(arr)
}

fn convert_tools(tools: &serde_json::Value) -> Option<serde_json::Value> {
    let arr = tools.as_array()?;
    let out: Vec<serde_json::Value> = arr
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            Some(serde_json::json!({
                "name": func.get("name"),
                "description": func.get("description"),
                "input_schema": func.get("parameters"),
            }))
        })
        .collect();
    Some(serde_json::Value::Array(out))
}
