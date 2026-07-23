//! Test harness: boot the daemon on an ephemeral port, return its address and a
//! handle the caller can abort to "kill" the server. Thin wrapper over the same
//! `serve::bind_and_serve` path the shipping binary uses.

use std::net::SocketAddr;

use tokio::task::JoinHandle;

pub async fn serve() -> (SocketAddr, JoinHandle<()>) {
    let dir = crate::state::state_dir();
    match crate::serve::bind_and_serve(&dir, Some(0)).await {
        Ok(v) => v,
        Err(e) => panic!("testkit serve failed: {e}"),
    }
}
