//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod init;
mod repo;
mod status;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "travsr",
    version,
    about = "The code graph that lives next to git."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialise a Travsr index in the current repository.
    Init,
    /// Start the Travsr daemon (git hook + file watcher + MCP server).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Start the MCP server in the foreground.
    Mcp {
        /// Use stdio transport (for local IDE / agent integration).
        #[arg(long)]
        stdio: bool,
    },
    /// Print index and graph status.
    Status,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    Start,
    Stop,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Init => init::run()?,
        Command::Daemon { action } => match action {
            DaemonAction::Start => travsr_daemon::Daemon::run()?,
            DaemonAction::Stop | DaemonAction::Status => {
                tracing::info!("travsr daemon {:?}: stub — Sprint 3", action);
            }
        },
        Command::Mcp { stdio: _ } => {
            tracing::info!("travsr mcp: stub — Sprint 3");
        }
        Command::Status => status::run()?,
    }
    Ok(())
}
