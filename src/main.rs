mod cli;
mod irc;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Cli::parse();
    match args.command.unwrap_or(Command::Run) {
        Command::Run => irc::serve("127.0.0.1:6667").await,
        Command::InstallIrssi { force, dry_run } => cli::install_irssi(force, dry_run),
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("matrirc=info")))
        .with(fmt::layer().with_target(true).with_level(true))
        .init();
}
