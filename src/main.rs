#![recursion_limit = "256"]

mod cli;
mod config;
mod irc;
mod matrix;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Cli::parse();
    match args.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
        Command::InstallIrssi { force, dry_run } => cli::install_irssi(force, dry_run),
        Command::Login { mxid, homeserver } => cli::login(&mxid, homeserver.as_deref()).await,
    }
}

async fn run() -> Result<()> {
    use tracing::warn;

    let cfg_path = config::config_path()?;
    let matrix_handle = match config::Config::load(&cfg_path) {
        Ok(cfg) => {
            tracing::info!("matrix: config loaded from {}", cfg_path.display());
            Some(tokio::spawn(async move {
                if let Err(e) = matrix::run_sync(cfg).await {
                    warn!("matrix sync error: {e:#}");
                }
            }))
        }
        Err(e) => {
            warn!(
                "no matrix config at {} ({e}); run `matrirc login` to enable sync",
                cfg_path.display()
            );
            None
        }
    };

    let irc_result = irc::serve("127.0.0.1:6667").await;
    if let Some(h) = matrix_handle {
        h.abort();
    }
    irc_result
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("matrirc=info")))
        .with(fmt::layer().with_target(true).with_level(true))
        .init();
}
