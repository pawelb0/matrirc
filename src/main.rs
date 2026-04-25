#![recursion_limit = "256"]

mod bridge;
mod cli;
mod config;
mod daemon;
mod irc;
mod matrix;
mod names;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Cli::parse();
    match args.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
        Command::InstallIrssi { force, dry_run, bin } => cli::install_irssi(force, dry_run, bin),
        Command::Login { mxid, homeserver, token, skip_verify } => {
            cli::login(&mxid, homeserver.as_deref(), token, skip_verify).await
        }
        Command::BootstrapE2ee => matrix::bootstrap_e2ee(cli::read_recovery_key()?).await,
        Command::Reset { force } => cli::reset(force),
        Command::Verify => cli::verify().await,
        Command::Status => cli::status(),
        Command::Stop => cli::stop(),
    }
}

async fn run() -> Result<()> {
    use std::sync::Arc;
    use tracing::warn;

    let _pid_guard = daemon::claim()?;

    let env_override_room = bridge::env_override();
    let (bridge_state, to_matrix_rx) = bridge::Bridge::new(bridge::Mapping::default());
    match &env_override_room {
        Some(room) => tracing::info!("bridge: MATRIRC_ROOM set, only bridging {room}"),
        None => tracing::info!("bridge: auto-discovering all Joined rooms after sync"),
    }

    let store_path = names::default_store_path()?;
    let name_store = Arc::new(names::NameStore::load(store_path)?);

    let cfg_path = config::config_path()?;
    let matrix_handle = match config::Config::load(&cfg_path) {
        Ok(cfg) => {
            tracing::info!("matrix: config loaded from {}", cfg_path.display());
            let b = bridge_state.clone();
            let ns = name_store.clone();
            let only = env_override_room.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = matrix::run_sync(cfg, b, to_matrix_rx, ns, only).await {
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

    let bind = std::env::var("MATRIRC_BIND").unwrap_or_else(|_| "127.0.0.1:6667".into());
    let serve = irc::serve(&bind, bridge_state);
    tokio::pin!(serve);
    let result = tokio::select! {
        r = &mut serve => r,
        _ = wait_for_signal() => {
            tracing::info!("received shutdown signal, exiting");
            Ok(())
        }
    };
    if let Some(h) = matrix_handle {
        h.abort();
    }
    result
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) { Ok(s) => s, Err(_) => return };
    let mut int = match signal(SignalKind::interrupt()) { Ok(s) => s, Err(_) => return };
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("matrirc=info")))
        .with(fmt::layer().with_target(true).with_level(true))
        .init();
}
