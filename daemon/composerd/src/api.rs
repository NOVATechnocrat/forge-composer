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
    pub budget_overrides: std::sync::Mutex<std::collections::HashMap<String, f64>>,
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
        .route("/roles", get(roles))
        .route("/sessions", post(create_session).get(list_sessions))
        .route("/sessions/detail", get(sessions_detail))
        .route("/sessions/{id}/events", get(events))
        .route("/sessions/{id}/stream", get(stream))
        .route("/sessions/{id}/message", post(post_message))
        .route("/sessions/{id}/role", post(set_role))
        .route("/sessions/{id}/approve", post(approve))
        .route("/sessions/{id}/pause", post(pause))
        .route("/sessions/{id}/resume", post(resume))
        .route("/sessions/{id}/steer", post(steer))
        .route("/sessions/{id}/inject", post(inject))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/checkpoints", get(checkpoints))
        .route("/sessions/{id}/restore", post(restore))
        .route("/sessions/{id}/file_at", get(file_at))
        .route("/sessions/{id}/diff", get(diff))
        .route("/sessions/{id}/adopt", post(adopt))
        .route("/sessions/{id}/budget", post(budget))
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
    #[serde(default)]
    pub role: Option<String>,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    body: Option<axum::Json<CreateSessionBody>>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let (workspace, role) = match body {
        Some(b) => (b.0.workspace, b.0.role),
        None => (None, None),
    };
    let workspace = workspace
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let role = role.filter(|s| !s.is_empty()).unwrap_or_else(|| "orchestrator".to_string());
    if !state.cfg.roles.contains_key(&role) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown role: {role}"),
        ));
    }
    let id = state
        .store
        .create_session()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut meta = crate::state::SessionMeta::orchestrator(workspace);
    meta.role = role;
    let _ = crate::state::write_meta(&state.state_dir, &id, &meta);
    Ok(axum::Json(serde_json::json!({"id": id})))
}

/// `GET /roles` — every configured role as `{name, provider, model}`, sorted by
/// name. `roles` is a BTreeMap so iteration is already name-sorted.
async fn roles(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let mut out = Vec::with_capacity(state.cfg.roles.len());
    for (name, rc) in &state.cfg.roles {
        out.push(serde_json::json!({
            "name": name,
            "provider": rc.provider,
            "model": rc.model,
        }));
    }
    Ok(axum::Json(serde_json::json!({"roles": out})))
}

#[derive(serde::Deserialize)]
pub struct RoleBody {
    pub role: String,
}

/// `POST /sessions/{id}/role` — switch a session's role mid-flight. The role
/// must exist in config (400 otherwise); `meta.role` is updated AND persisted,
/// and a `role_switch` event (actor:human, from/to) is ledgered BEFORE the next
/// turn runs, so the switch is never invisible. `run_turn` reads `meta.role`
/// fresh at turn start, so the next turn resolves the new role's chain.
async fn set_role(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<RoleBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let new_role = body.role.clone();
    if !state.cfg.roles.contains_key(&new_role) {
        return Err((StatusCode::BAD_REQUEST, format!("unknown role: {new_role}")));
    }
    let mut meta = crate::state::load_meta(&state.state_dir, &id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_else(|| {
            crate::state::SessionMeta::orchestrator(std::env::current_dir().unwrap_or_default())
        });
    let old_role = meta.role.clone();
    if old_role != new_role {
        meta.role = new_role.clone();
        let _ = crate::state::write_meta(&state.state_dir, &id, &meta);
        let _ = state.append_event(
            &id,
            "human",
            "role_switch",
            "trusted",
            serde_json::json!({"from": old_role, "to": new_role}),
        );
    }
    Ok(axum::Json(serde_json::json!({"ok": true})))
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

async fn sessions_detail(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let ids = state
        .store
        .list_sessions()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut out = Vec::new();
    for id in ids {
        let meta = crate::state::load_meta(&state.state_dir, &id).ok().flatten();
        let events = state.store.read(&id, 0).unwrap_or_default();
        let (mut p, mut c, mut cost) = (0u64, 0u64, 0f64);
        for e in events.iter().filter(|e| e.kind == "usage") {
            p += e.body.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            c += e.body.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            cost += e.body.get("cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
        }
        let status = if state.is_running(&id) {
            "running"
        } else if state
            .control_for(&id)
            .paused
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            "paused"
        } else {
            "idle"
        };
        out.push(serde_json::json!({
            "id": id,
            "kind": meta.as_ref().map(|m| m.kind.clone()).unwrap_or_else(|| "orchestrator".into()),
            "parent": meta.as_ref().and_then(|m| m.parent.clone()),
            "role": meta.as_ref().map(|m| m.role.clone()).unwrap_or_else(|| "orchestrator".into()),
            "title": meta.as_ref().and_then(|m| m.title.clone()),
            "status": status,
            "prompt_tokens": p,
            "completion_tokens": c,
            "cost_usd": cost,
        }));
    }
    Ok(axum::Json(serde_json::json!({"sessions": out})))
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

/// An attachment may be path-based (read from the jail at fold time — the M1
/// sealed form) or name/content-based (inline data — the M4 form). Both shapes
/// deserialize; serialization skips absent fields so existing events stay
/// byte-identical.
#[derive(Deserialize, serde::Serialize)]
pub struct Attachment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Hard cap on total inline attachment `content` bytes per message (256 KiB).
const ATTACH_TOTAL_CAP: usize = 262_144;

async fn post_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<MessageBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    if let Some(atts) = &body.attachments {
        let total: usize = atts
            .iter()
            .filter_map(|a| a.content.as_deref())
            .map(|c| c.len())
            .sum();
        if total > ATTACH_TOTAL_CAP {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "attachment content exceeds 262144 bytes".into(),
            ));
        }
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

#[derive(Deserialize)]
pub struct DiffQuery {
    pub from: String,
}

async fn diff(
    State(state): State<Arc<AppState>>,
    Path(session): Path<String>,
    Query(q): Query<DiffQuery>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let (shadow, _jail) = crate::orchestrator::session_shadow(&state, &session)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match shadow.diff(&q.from) {
        Ok(patch) => Ok(axum::Json(serde_json::json!({"patch": patch}))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct AdoptBody {
    pub child: String,
}

/// Run `git -C <cwd> <args...>`; returns Err with stderr on non-zero exit.
fn git_in(cwd: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), err.trim());
    }
    Ok(())
}

/// Run `git -C <cwd> <args...>`; returns stdout on success, Err with stderr.
fn git_in_out(cwd: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Human-only adoption: commit a child subagent's worktree work, merge its
/// branch into the parent workspace, remove the worktree, delete the branch,
/// and ledger an `adopt` event on the parent. No model turn is triggered.
async fn adopt(
    State(state): State<Arc<AppState>>,
    Path(parent): Path<String>,
    body: axum::Json<AdoptBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&parent) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let child = &body.child;
    let child_meta = crate::state::load_meta(&state.state_dir, child)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "unknown child session".into()))?;
    if child_meta.kind != "subagent" {
        return Err((StatusCode::BAD_REQUEST, "not a subagent".into()));
    }
    if child_meta.parent.as_deref() != Some(parent.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "child does not belong to this parent".into()));
    }
    let worktree = match child_meta.worktree.as_deref() {
        Some(w) => w,
        None => return Err((StatusCode::BAD_REQUEST, "child has no worktree".into())),
    };
    let parent_meta = crate::state::load_meta(&state.state_dir, &parent)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_else(|| crate::state::SessionMeta::orchestrator(std::path::PathBuf::from(".")));
    let parent_ws = parent_meta.workspace.clone();
    let branch = format!("fc/{}", child.to_lowercase());

    // 1. Commit the child's dirty worktree as composer.
    let dirty = git_in_out(worktree, &["status", "--porcelain"]).unwrap_or_default();
    if !dirty.trim().is_empty() {
        git_in(worktree, &["add", "-A"]).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let msg = format!("adopt {child}: subagent work");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["-c", "user.name=composer", "-c", "user.email=composer@forge"])
            .args(["commit", "-q", "-m", &msg])
            .output()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("commit child work: {err}")));
        }
    }

    // 2. Merge the child branch into the parent workspace (no-ff).
    let title = child_meta.title.clone().unwrap_or_default();
    let merge_msg = if title.is_empty() {
        format!("adopt {child}")
    } else {
        format!("adopt {child} ({title})")
    };
    let merge_out = std::process::Command::new("git")
        .arg("-C")
        .arg(&parent_ws)
        .args(["merge", "--no-ff", "-m", &merge_msg, &branch])
        .output()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !merge_out.status.success() {
        // Conflict: abort, leave the worktree intact, return 409.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&parent_ws)
            .args(["merge", "--abort"])
            .output();
        let err = String::from_utf8_lossy(&merge_out.stderr);
        return Err((
            StatusCode::CONFLICT,
            serde_json::json!({"error": "merge conflict", "child": child, "detail": err.trim()}).to_string(),
        ));
    }
    let merge_commit = git_in_out(&parent_ws, &["rev-parse", "HEAD"])
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .trim()
        .to_string();

    // 3. Remove the worktree + delete the branch.
    if let Err(e) = tools::worktree::remove(&parent_ws, worktree) {
        // Non-fatal: the merge succeeded; surface but keep going.
        let _ = state.append_event(
            &parent,
            "system",
            "error",
            "trusted",
            serde_json::json!({"error": format!("worktree remove: {e}")}),
        );
    }
    let _ = git_in(&parent_ws, &["branch", "-d", &branch])
        .or_else(|_| git_in(&parent_ws, &["branch", "-D", &branch]));

    // 4. Ledger the adoption on the parent (human action, trusted).
    let _ = state.append_event(
        &parent,
        "human",
        "adopt",
        "trusted",
        serde_json::json!({
            "child": child,
            "branch": branch,
            "merge_commit": merge_commit,
        }),
    );

    Ok(axum::Json(serde_json::json!({"merge_commit": merge_commit})))
}

#[derive(Deserialize)]
pub struct BudgetBody {
    pub session_usd: f64,
}

/// Human-only budget raise: set a per-session override, ledger a `budget`
/// event (action:"raised"), and un-pause the session (resuming pending input).
async fn budget(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: axum::Json<BudgetBody>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    if !state.store.session_exists(&id) {
        return Err((StatusCode::NOT_FOUND, "unknown session".into()));
    }
    let limit = body.session_usd;
    if !limit.is_finite() || limit <= 0.0 {
        return Err((StatusCode::BAD_REQUEST, "session_usd must be a finite positive number".into()));
    }
    {
        let mut map = state.budget_overrides.lock().unwrap();
        map.insert(id.clone(), limit);
    }
    let _ = state.append_event(
        &id,
        "human",
        "budget",
        "trusted",
        serde_json::json!({"action": "raised", "limit_usd": limit}),
    );
    // Un-pause (resume semantics, including pending input).
    let ctl = state.control_for(&id);
    ctl.paused.store(false, std::sync::atomic::Ordering::SeqCst);
    if !state.is_running(&id) {
        if let Ok(evs) = state.store.read(&id, 0) {
            if crate::api::has_pending_input(&evs) {
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
