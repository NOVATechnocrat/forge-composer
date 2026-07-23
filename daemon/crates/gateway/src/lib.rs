//! gateway — OpenAI-compatible streaming chat adapter.

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ChatUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug)]
pub struct ChatResult {
    pub content: String,
    pub usage: Option<ChatUsage>,
}

/// POST `{base_url}/chat/completions` with `stream:true`, parse SSE `data:` lines,
/// concatenate `choices[0].delta.content`, call `on_delta` per fragment, capture a
/// trailing usage object if the provider sends one, stop at `[DONE]`.
pub async fn chat_stream(
    cfg: &ProviderConfig,
    messages: &[ChatMessage],
    mut on_delta: impl FnMut(&str),
) -> anyhow::Result<ChatResult> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": cfg.model,
        "stream": true,
        "messages": messages,
    });
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

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("stream read failed: {e}"))?;
        buf.extend_from_slice(&chunk);
        // Process complete SSE frames separated by "\n\n"
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
                    return Ok(ChatResult { content, usage });
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
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                {
                    content.push_str(delta);
                    on_delta(delta);
                }
                if let Some(u) = v.get("usage") {
                    if let Ok(parsed) = serde_json::from_value::<ChatUsage>(u.clone()) {
                        usage = Some(parsed);
                    }
                }
            }
        }
    }

    // Stream ended without explicit [DONE]; return what we have.
    Ok(ChatResult { content, usage })
}
