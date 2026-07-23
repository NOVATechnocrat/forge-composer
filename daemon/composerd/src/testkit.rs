//! Test harness: boot the daemon on an ephemeral port, return its address and a
//! handle the caller can abort to "kill" the server.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::api::{build_router, AppState};
use crate::config;
use crate::state as st;

pub async fn serve() -> (SocketAddr, JoinHandle<()>) {
    let dir = st::state_dir();
    let cfg = config::load_or_init(&dir).expect("load_or_init");
    let token = st::ensure_auth_token(&dir).expect("ensure_auth_token");
    let redactor = ledger::Redactor::new(config::secrets(&cfg));
    let store = ledger::SessionStore::new(dir.join("sessions"), redactor);

    let state = Arc::new(AppState {
        token,
        store,
        cfg,
        channels: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let app = build_router(state).await;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}
