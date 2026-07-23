//! gateway — OpenAI-compatible streaming chat adapter with tool-call support,
//! plus an Anthropic Messages adapter behind one `chat()` entry point.

pub mod anthropic;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ProviderKind {
    #[default]
    OpenAI,
    Anthropic,
}

pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub kind: ProviderKind,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }

    pub fn assistant_tool_calls(content: &str, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ChatUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Default)]
pub struct ChatResult {
    pub content: String,
    pub usage: Option<ChatUsage>,
    pub tool_calls: Vec<ToolCall>,
}

/// Single entry point: dispatches on `cfg.kind`. Anthropic arm lands in Task 5.
pub async fn chat(
    cfg: &ProviderConfig,
    messages: &[ChatMessage],
    tools: Option<&serde_json::Value>,
    on_delta: impl FnMut(&str),
) -> anyhow::Result<ChatResult> {
    match cfg.kind {
        ProviderKind::OpenAI => chat_stream(cfg, messages, tools, on_delta).await,
        ProviderKind::Anthropic => anthropic::chat_anthropic(cfg, messages, tools, on_delta).await,
    }
}

/// POST `{base_url}/chat/completions` with `stream:true`, parse SSE `data:`
/// lines, concatenate `choices[0].delta.content`, accumulate streamed
/// `tool_calls` by index, call `on_delta` per text fragment, capture a trailing
/// usage object if present, stop at `[DONE]`.
pub async fn chat_stream(
    cfg: &ProviderConfig,
    messages: &[ChatMessage],
    tools: Option<&serde_json::Value>,
    mut on_delta: impl FnMut(&str),
) -> anyhow::Result<ChatResult> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": cfg.model,
        "stream": true,
        "messages": build_openai_messages(messages),
    });
    if let Some(t) = tools {
        body["tools"] = t.clone();
    }
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| anyhow::anyhow!("http client build failed: {e}"))?;

    let mut req = client.post(&url).json(&body);
    if let Some(key) = &cfg.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("chat request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("chat completions returned {status}: {text}");
    }

    let mut stream = resp.bytes_stream();
    use futures::StreamExt;

    let mut buf = Vec::<u8>::new();
    let mut content = String::new();
    let mut usage: Option<ChatUsage> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();

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
                if payload == "[DONE]" {
                    return Ok(ChatResult {
                        content,
                        usage,
                        tool_calls,
                    });
                }
                if payload.is_empty() {
                    continue;
                }
                let v: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(delta) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                {
                    if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                        content.push_str(c);
                        on_delta(c);
                    }
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let index = tc
                                .get("index")
                                .and_then(|i| i.as_u64())
                                .unwrap_or(0) as usize;
                            if tool_calls.len() <= index {
                                tool_calls.resize_with(index + 1, || ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: String::new(),
                                });
                            }
                            let entry = &mut tool_calls[index];
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                entry.id = id.to_string();
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                    entry.name = name.to_string();
                                }
                                if let Some(args) =
                                    func.get("arguments").and_then(|a| a.as_str())
                                {
                                    entry.arguments.push_str(args);
                                }
                            }
                        }
                    }
                }
                if let Some(u) = v.get("usage") {
                    if let Ok(parsed) = serde_json::from_value::<ChatUsage>(u.clone()) {
                        usage = Some(parsed);
                    }
                }
            }
        }
    }

    Ok(ChatResult {
        content,
        usage,
        tool_calls,
    })
}

/// Build the OpenAI wire-shape messages array from `ChatMessage`, converting
/// `tool_calls` / `tool_call_id` to the format the provider expects.
fn build_openai_messages(messages: &[ChatMessage]) -> serde_json::Value {
    let arr = messages
        .iter()
        .map(|m| {
            let mut o = serde_json::json!({ "role": m.role });
            if m.role == "tool" {
                o["tool_call_id"] =
                    serde_json::Value::String(m.tool_call_id.clone().unwrap_or_default());
                o["content"] = serde_json::Value::String(m.content.clone());
            } else if let Some(calls) = &m.tool_calls {
                o["content"] = if m.content.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(m.content.clone())
                };
                let tc: Vec<serde_json::Value> = calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments}
                        })
                    })
                    .collect();
                o["tool_calls"] = serde_json::Value::Array(tc);
            } else {
                o["content"] = serde_json::Value::String(m.content.clone());
            }
            o
        })
        .collect();
    serde_json::Value::Array(arr)
}
