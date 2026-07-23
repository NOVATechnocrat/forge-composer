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
    pub channels:
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::broadcast::Sender<Frame>>>,
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
}

pub async fn build_router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/stream", get(stream))
        .route("/sessions/{id}/message", post(post_message))
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

#[derive(serde::Deserialize)]
pub struct CreateBody {}

async fn create_session(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let id = state
        .store
        .create_session()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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

#[derive(Deserialize)]
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
    text: String,
}

async fn post_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<MessageBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let ev = state
        .append_event(
            &id,
            "human",
            "message",
            "trusted",
            serde_json::json!({"text": body.text}),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let st = state.clone();
    tokio::spawn(async move {
        crate::orchestrator::run_turn(st, id).await;
    });
    Ok(axum::Json(serde_json::json!({"seq": ev.seq})))
}
