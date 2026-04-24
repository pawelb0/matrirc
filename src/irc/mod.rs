pub mod conn;
pub mod proto;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use crate::bridge::Bridge;

pub async fn serve(addr: &str, bridge: Bridge) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    info!(%addr, "matrirc IRC listening");
    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        debug!(%peer, "accept");
        let b = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) = conn::handle(sock, peer, b).await {
                error!(%peer, error = %e, "connection error");
            }
        });
    }
}
