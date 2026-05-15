//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod ask;
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
    /// Look up callers and dependencies for a symbol name.
    Ask {
        /// Symbol name to search for (partial match supported).
        query: String,
    },
    /// Re-index a list of changed files (invoked by the git hook).
    #[command(hide = true)]
    HookRun {
        /// Paths reported by `git diff --name-only`.
        paths: Vec<String>,
    },
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
            let cwd = std::env::current_dir()?;
            let repo_root = repo::find_git_root(&cwd)?;
            let db_path = repo_root.join(".travsr/graph.db");
            if !db_path.exists() {
                anyhow::bail!("not initialized — run `travsr init` first");
            }
            travsr_mcp::serve_stdio(&db_path)?;
        }
        Command::Status => status::run()?,
        Command::Ask { query } => ask::run(&query)?,
        Command::HookRun { paths } => {
            let cwd = std::env::current_dir()?;
            let repo_root = repo::find_git_root(&cwd)?;
            let mut store = {
                let db_path = repo_root.join(".travsr/graph.db");
                travsr_store::SqliteStore::open(&db_path)?
            };
            let abs_paths: Vec<std::path::PathBuf> =
                paths.iter().map(|p| repo_root.join(p)).collect();
            travsr_daemon::reindex_files(&abs_paths, &repo_root, &mut store)?;
        }
    }
    Ok(())
}
