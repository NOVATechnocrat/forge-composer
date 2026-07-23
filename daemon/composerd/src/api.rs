//! axum HTTP+SSE API, bearer auth, session + event routes.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::stream::StreamExt;
use serde::Deserialize;

/// Constant-time byte comparison for the bearer token.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub struct AppState {
    pub token: String,
    pub store: ledger::SessionStore,
    pub cfg: crate::config::Config,
    pub state_dir: std::path::PathBuf,
    pub channels:
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::broadcast::Sender<Frame>>>,
    pub approvals:
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    pub controls:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<SessionControl>>>,
}

/// Per-session control plane: a pause flag the agent loop polls at every tool
/// boundary, and the abort handle of the currently-running turn task (if any).
pub struct SessionControl {
    pub paused: std::sync::atomic::AtomicBool,
    pub abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl SessionControl {
    fn new() -> Self {
        Self {
            paused: std::sync::atomic::AtomicBool::new(false),
            abort: std::sync::Mutex::new(None),
        }
    }
}

#[derive(Clone)]
pub enum Frame {
    Ledger(ledger::Event),
    Delta(String),
}

impl AppState {
    pub fn append_event(
        &self,
        session: &str,
        actor: &str,
        kind: &str,
        provenance: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<ledger::Event> {
        let event = self.store.append(session, actor, kind, provenance, body)?;
        self.broadcast(session, Frame::Ledger(event.clone()));
        Ok(event)
    }

    pub fn broadcast(&self, session: &str, frame: Frame) {
        if let Ok(map) = self.channels.lock() {
            if let Some(tx) = map.get(session) {
                let _ = tx.send(frame);
            }
        }
    }

    pub fn channel_for(&self, session: &str) -> tokio::sync::broadcast::Sender<Frame> {
        let mut map = self.channels.lock().unwrap();
        map.entry(session.to_string())
            .or_insert_with(|| {
                let (tx, _rx) = tokio::sync::broadcast::channel(256);
                tx
            })
            .clone()
    }

    pub fn control_for(&self, session: &str) -> std::sync::Arc<SessionControl> {
        let mut map = self.controls.lock().unwrap();
        map.entry(session.to_string())
            .or_insert_with(|| std::sync::Arc::new(SessionControl::new()))
            .clone()
    }

    /// True iff a turn task is currently registered and not yet finished.
    pub fn is_running(&self, session: &str) -> bool {
        let ctl = self.control_for(session);
        let guard = ctl.abort.lock().unwrap();
        match &*guard {
            Some(h) => !h.is_finished(),
            None => false,
        }
    }
}

/// True iff a `message` (from human) or `steer` event is newer than the last
/// agent reply — i.e. there is input the agent has not yet answered.
pub fn has_pending_input(events: &[ledger::Event]) -> bool {
    let last_agent_reply = events
        .iter()
        .rev()
        .find(|e| e.kind == "message" && e.actor != "human")
        .map(|e| e.seq)
        .unwrap_or(0);
    events.iter().any(|e| {
        e.seq > last_agent_reply
            && (e.kind == "steer" || (e.kind == "message" && e.actor == "human"))
    })
}

pub async fn build_router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/stream", get(stream))
        .route("/sessions/{id}/message", post(post_message))
        .route("/sessions/{id}/approve", post(approve))
        .route("/sessions/{id}/pause", post(pause))
        .route("/sessions/{id}/resume", post(resume))
        .route("/sessions/{id}/steer", post(steer))
        .route("/sessions/{id}/inject", post(inject))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/checkpoints", get(checkpoints))
        .route("/sessions/{id}/restore", post(restore))
        .route("/sessions/{id}/file_at", get(file_at))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({"name":"composerd","version":"0.1.0"}))
}

pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let provided = header.strip_prefix("Bearer ").unwrap_or("");
    if ct_eq(provided.as_bytes(), state.token.as_bytes()) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

#[derive(serde::Deserialize, Default)]
pub struct CreateSessionBody {
    #[serde(default)]
    pub workspace: Option<String>,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Option<axum::Json<CreateSessionBody>>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let workspace = body
        .and_then(|b| b.0.workspace)
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let id = state
        .store
        .create_session()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let meta = crate::state::SessionMeta::orchestrator(workspace);
    let _ = crate::state::write_meta(&state.state_dir, &id, &meta);
    Ok(axum::Json(serde_json::json!({"id": id})))
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let sessions = state
        .store
        .list_sessions()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(axum::Json(serde_json::json!({"sessions": sessions})))
}

#[derive(serde::Deserialize)]
pub struct SinceQuery {
    since: Option<u64>,
}

async fn events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<SinceQuery>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let since = q.since.unwrap_or(0);
    let evs = state
        .store
        .read(&id, since)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    Ok(axum::Json(serde_json::json!({"events": evs})))
}

async fn stream(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if !state.store.session_exists(&id) {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    }
    let tx = state.channel_for(&id);
    let rx = tx.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(frame) => Some((frame, rx)),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some((
                Frame::Delta(String::new()),
                rx,
            )),
        }
    })
    .map(|frame| {
        let ev = match frame {
            Frame::Ledger(e) => SseEvent::default()
                .event("ledger")
                .data(serde_json::to_string(&e).unwrap_or_default()),
            Frame::Delta(t) => SseEvent::default()
                .event("delta")
                .data(serde_json::to_string(&serde_json::json!({"text": t})).unwrap_or_default()),
        };
        Ok::<_, std::convert::Infallible>(ev)
    });
    Sse::new(stream).into_response()
}

#[derive(Deserialize)]
pub struct MessageBody {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Deserialize, serde::Serialize)]
pub struct Attachment {
    pub path: String,
}

async fn post_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<MessageBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let mut msg_body = serde_json::json!({"text": body.text});
    if let Some(atts) = &body.attachments {
        if !atts.is_empty() {
            msg_body["attachments"] = serde_json::json!(atts);
        }
    }
    let ev = state
        .append_event(&id, "human", "message", "trusted", msg_body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let ctl = state.control_for(&id);
    if !ctl.paused.load(std::sync::atomic::Ordering::SeqCst) {
        let st = state.clone();
        let idc = id.clone();
        tokio::spawn(async move {
            crate::orchestrator::run_turn(st, idc).await;
        });
    }
    Ok(axum::Json(serde_json::json!({"seq": ev.seq})))
}

#[derive(Deserialize)]
pub struct ApproveBody {
    pub id: String,
    pub approved: bool,
}

async fn approve(
    State(state): State<Arc<AppState>>,
    Path(session): Path<String>,
    body: axum::Json<ApproveBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&session) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let tx = {
        let mut map = state.approvals.lock().unwrap();
        map.remove(&body.id)
    };
    let tx = match tx {
        Some(t) => t,
        None => return Err((StatusCode::NOT_FOUND, "unknown approval id".into())),
    };
    let _ = state.append_event(
        &session,
        "system",
        "approval_decision",
        "trusted",
        serde_json::json!({"id": body.id, "approved": body.approved, "by": "human"}),
    );
    let _ = tx.send(body.approved);
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct TextBody {
    pub text: String,
}

async fn pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    state
        .control_for(&id)
        .paused
        .store(true, std::sync::atomic::Ordering::SeqCst);
    state
        .append_event(&id, "human", "pause", "trusted", serde_json::json!({}))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let ctl = state.control_for(&id);
    ctl.paused.store(false, std::sync::atomic::Ordering::SeqCst);
    state
        .append_event(&id, "human", "resume", "trusted", serde_json::json!({}))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !state.is_running(&id) {
        if let Ok(evs) = state.store.read(&id, 0) {
            if has_pending_input(&evs) {
                let st = state.clone();
                let idc = id.clone();
                tokio::spawn(async move {
                    crate::orchestrator::run_turn(st, idc).await;
                });
            }
        }
    }
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn steer(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<TextBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    state
        .append_event(
            &id,
            "human",
            "steer",
            "trusted",
            serde_json::json!({"text": body.text}),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let ctl = state.control_for(&id);
    if !state.is_running(&id) && !ctl.paused.load(std::sync::atomic::Ordering::SeqCst) {
        let st = state.clone();
        let idc = id.clone();
        tokio::spawn(async move {
            crate::orchestrator::run_turn(st, idc).await;
        });
    }
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn inject(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<TextBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    state
        .append_event(
            &id,
            "human",
            "context_inject",
            "trusted",
            serde_json::json!({"text": body.text}),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn interrupt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let ctl = state.control_for(&id);
    if let Some(h) = ctl.abort.lock().unwrap().take() {
        h.abort();
    }
    state
        .append_event(&id, "human", "interrupt", "trusted", serde_json::json!({}))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

async fn checkpoints(
    State(state): State<Arc<AppState>>,
    Path(session): Path<String>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let (shadow, _jail) = crate::orchestrator::session_shadow(&state, &session)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let list = shadow
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let arr: Vec<serde_json::Value> = list
        .into_iter()
        .map(|(hash, label)| serde_json::json!({"hash": hash, "label": label}))
        .collect();
    Ok(axum::Json(serde_json::json!({"checkpoints": arr})))
}

#[derive(Deserialize)]
pub struct RestoreBody {
    pub hash: String,
}

async fn restore(
    State(state): State<Arc<AppState>>,
    Path(session): Path<String>,
    body: axum::Json<RestoreBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let (shadow, _jail) = crate::orchestrator::session_shadow(&state, &session)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    shadow
        .restore(&body.hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = state.append_event(
        &session,
        "system",
        "message",
        "trusted",
        serde_json::json!({"text": format!("restored checkpoint {}", body.hash)}),
    );
    Ok(axum::Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct FileAtQuery {
    pub hash: String,
    pub path: String,
}

async fn file_at(
    State(state): State<Arc<AppState>>,
    Path(session): Path<String>,
    Query(q): Query<FileAtQuery>,
) -> Result<Response, (StatusCode, String)> {
    let (shadow, _jail) = crate::orchestrator::session_shadow(&state, &session)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match shadow.file_at(&q.hash, &q.path) {
        Ok(content) => Ok(([(header::CONTENT_TYPE, "text/plain")], content).into_response()),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, kind: &str, actor: &str) -> ledger::Event {
        ledger::Event {
            v: "forgeloop.composer.event.v1".into(),
            seq,
            ts: "t".into(),
            session: "s".into(),
            actor: actor.into(),
            kind: kind.into(),
            provenance: "trusted".into(),
            body: serde_json::json!({}),
        }
    }

    #[test]
    fn pending_input_rule() {
        // human asked, agent answered: nothing pending
        assert!(!has_pending_input(&[ev(1, "message", "human"), ev(2, "message", "orchestrator")]));
        // human message after agent reply: pending
        assert!(has_pending_input(&[
            ev(1, "message", "human"),
            ev(2, "message", "orchestrator"),
            ev(3, "message", "human")
        ]));
        // steer after agent reply: pending
        assert!(has_pending_input(&[ev(1, "message", "orchestrator"), ev(2, "steer", "human")]));
        // inject alone does NOT wake
        assert!(!has_pending_input(&[
            ev(1, "message", "orchestrator"),
            ev(2, "context_inject", "human")
        ]));
    }
}
