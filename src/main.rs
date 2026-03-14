mod irc;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    irc::serve("127.0.0.1:6667").await
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("matrirc=info")))
        .with(fmt::layer().with_target(true).with_level(true))
        .init();
}
