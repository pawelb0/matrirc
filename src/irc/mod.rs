pub mod conn;
pub mod proto;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tracing::{error, info};

pub async fn serve(addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    info!(%addr, "matrirc IRC listening");
    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        info!(%peer, "client connected");
        tokio::spawn(async move {
            if let Err(e) = conn::handle(sock, peer).await {
                error!(%peer, error = %e, "connection error");
            }
        });
    }
}
