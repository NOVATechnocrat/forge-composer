//! orchestrator — the M0 single-agent turn loop.

use std::sync::Arc;

use crate::api::AppState;

const SYSTEM_PROMPT: &str = "You are Forge Composer's orchestrator (M0 spine). Be concise.";

/// Rebuild history from the ledger, prepend the system prompt, call the gateway,
/// broadcast delta fragments live, then append the orchestrator `message` and
/// `usage` events. On any gateway error append an `error` event — never panic.
pub async fn run_turn(state: Arc<AppState>, session: String) {
    let events = match state.store.read(&session, 0) {
        Ok(e) => e,
        Err(e) => {
            let _ = state.append_event(
                &session,
                "system",
                "error",
                "trusted",
                serde_json::json!({"error": format!("read history: {e}")}),
            );
            return;
        }
    };

    let mut messages = vec![gateway::ChatMessage::text("system", SYSTEM_PROMPT)];
    for ev in events.iter() {
        if ev.kind != "message" {
            continue;
        }
        let role = match ev.actor.as_str() {
            "human" => "user",
            "orchestrator" => "assistant",
            _ => continue,
        };
        let text = ev
            .body
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        messages.push(gateway::ChatMessage::text(role, &text));
    }

    let cfg = match crate::config::resolve_role(&state.cfg, "orchestrator") {
        Ok(c) => c,
        Err(e) => {
            let _ = state.append_event(
                &session,
                "system",
                "error",
                "trusted",
                serde_json::json!({"error": format!("resolve_role: {e}")}),
            );
            return;
        }
    };

    let state_for_delta = state.clone();
    let session_for_delta = session.clone();
    let result = gateway::chat_stream(&cfg, &messages, None, |d| {
        state_for_delta.broadcast(&session_for_delta, crate::api::Frame::Delta(d.to_string()));
    })
    .await;

    match result {
        Ok(r) => {
            let _ = state.append_event(
                &session,
                "orchestrator",
                "message",
                "trusted",
                serde_json::json!({"text": r.content}),
            );
            if let Some(u) = r.usage {
                let _ = state.append_event(
                    &session,
                    "orchestrator",
                    "usage",
                    "trusted",
                    serde_json::json!({
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                    }),
                );
            }
        }
        Err(e) => {
            let _ = state.append_event(
                &session,
                "system",
                "error",
                "trusted",
                serde_json::json!({"error": format!("gateway: {e}")}),
            );
        }
    }
}
