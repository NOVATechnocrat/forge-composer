//! The real bind-and-serve path — shared by the `composerd serve` binary and the
//! testkit, so tests exercise the same wiring that ships.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::api::{build_router, AppState};
use crate::{config, state};

/// Bind 127.0.0.1 on `port_override` (or the config's `server.port` when `None`),
/// write `daemon.json` for client discovery, and serve in a spawned task.
pub async fn bind_and_serve(
    dir: &Path,
    port_override: Option<u16>,
) -> anyhow::Result<(SocketAddr, JoinHandle<()>)> {
    let cfg = config::load_or_init(dir)?;
    let token = state::ensure_auth_token(dir)?;
    let redactor = ledger::Redactor::new(config::secrets(&cfg));
    let store = ledger::SessionStore::new(dir.join("sessions"), redactor);

    let app_state = Arc::new(AppState {
        token,
        store,
        cfg: cfg.clone(),
        channels: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    let port = port_override.unwrap_or(cfg.server.port);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    state::write_daemon_json(dir, addr.port())?;

    let app = build_router(app_state).await;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((addr, handle))
}
