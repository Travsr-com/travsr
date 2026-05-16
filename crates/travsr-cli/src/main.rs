//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod ask;
mod graph;
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
    /// Show the dependency graph for a symbol or file as a tree or DOT.
    Graph {
        /// Symbol or file name to start from. Mutually exclusive with --all.
        query: Option<String>,
        /// Dump the entire indexed repository graph.
        #[arg(long)]
        all: bool,
        /// Maximum traversal depth (ignored with --all).
        #[arg(short, long, default_value = "3")]
        depth: u8,
        /// Which edges to follow (ignored with --all).
        #[arg(short = 'D', long, default_value = "deps")]
        direction: graph::Direction,
        /// Output format.
        #[arg(short, long, default_value = "tree")]
        format: graph::Format,
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
async fn main() {
    // Suppress the broken-pipe panic from `println!` when a pipe consumer
    // closes early (e.g. `travsr graph --all --format dot | head`).
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        if msg.contains("Broken pipe") || msg.contains("broken pipe") {
            std::process::exit(0);
        }
        eprintln!("{info}");
        std::process::exit(1);
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let result = run().await;

    // Flush stdout before exiting. Ignore broken pipe — the pipe consumer
    // closed early (e.g. `| head`), which is not an error on our side.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            if !is_broken_pipe(&e) {
                eprintln!("error: {e:#}");
            }
            std::process::exit(1);
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

async fn run() -> Result<()> {
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
        Command::Graph { query, all, depth, direction, format } => {
            match (all, query.as_deref()) {
                (true, Some(_)) => anyhow::bail!("--all and a query are mutually exclusive"),
                (true, None) => graph::run_all(format)?,
                (false, Some(q)) => graph::run(q, depth, direction, format)?,
                (false, None) => anyhow::bail!("provide a symbol/file query or pass --all"),
            }
        }
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
