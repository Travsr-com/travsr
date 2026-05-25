//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod ask;
mod graph;
mod index;
mod init;
mod migrate;
mod repo;
mod repos;
mod serve;
mod status;

use anyhow::{Context as _, Result};
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
        /// Serve all globally registered repos from ~/.travsr/registry.json.
        #[arg(long)]
        global: bool,
        /// Path to a specific graph.db file. Overrides automatic discovery from cwd.
        #[arg(long)]
        db: Option<std::path::PathBuf>,
    },
    /// List all globally registered repos.
    Repos,
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
    /// Index a directory of source files and emit a graph JSON (for CI / tooling).
    Index {
        /// Directory to index recursively.
        dir: std::path::PathBuf,
        /// Output path for the graph JSON.
        #[arg(long, short)]
        output: std::path::PathBuf,
        /// Corpus label for emitted nodes.
        #[arg(long, default_value = "ci")]
        corpus: String,
    },
    /// Re-index a list of changed files (invoked by the git hook).
    #[command(hide = true)]
    HookRun {
        /// Invoked from the git hook — reads changed files from git directly.
        /// Never passes filenames through the shell; prevents shell injection.
        #[arg(long)]
        from_hook: bool,
        /// Paths to re-index (used when calling hook-run directly, without --from-hook).
        paths: Vec<String>,
    },
    /// Migrate the graph store to a different backend (e.g. kuzu).
    Migrate {
        /// Target backend. Currently supported: kuzu
        #[arg(long = "to", value_name = "BACKEND")]
        to: String,
    },
    /// Start the SSE/HTTP MCP server for cloud and team deployments.
    Serve {
        /// TCP port to bind the SSE server on.
        #[arg(long, default_value = "3000")]
        port: u16,
        /// Directory containing tenant data subdirectories.
        #[arg(long)]
        tenants_dir: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start the daemon (detaches to background unless --foreground is given).
    Start {
        /// Run the daemon in the foreground (default: background).
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// Stop the running daemon.
    Stop,
    /// Check whether the daemon is running.
    Status,
    /// Stop the running daemon and start a fresh one.
    Restart,
}

#[tokio::main(flavor = "current_thread")]
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

    // Parse CLI args BEFORE initialising any subsystems.
    // Clap exits immediately for --version and --help via process::exit, so
    // init_tracing() (which may start the OTLP exporter or its background
    // tasks) must not run first — otherwise those simple queries hang.
    let cli = Cli::parse();

    init_tracing();

    let result = run(cli).await;

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

/// Initialise the global tracing subscriber.
///
/// Without the `otlp` feature: stderr-only JSON/pretty subscriber filtered by
/// `RUST_LOG` (default: `info`).
///
/// With the `otlp` feature: adds an OpenTelemetry OTLP layer that exports spans
/// via gRPC to `TRAVSR_OTLP_ENDPOINT` (default: `http://localhost:4317`).
/// This is off by default — only enable it when you have a collector running.
///
/// Log redaction: file contents are never logged. Spans record only paths,
/// counts, and numeric identifiers — never raw source text.
fn init_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    #[cfg(not(feature = "otlp"))]
    {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(env_filter)
            .init();
    }

    #[cfg(feature = "otlp")]
    {
        use opentelemetry_otlp::WithExportConfig as _;
        use tracing_subscriber::prelude::*;

        let endpoint = std::env::var("TRAVSR_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:4317".to_string());

        let exporter = opentelemetry_otlp::new_exporter()
            .tonic()
            .with_endpoint(&endpoint);

        // Gracefully degrade to stderr-only if the OTLP pipeline setup fails
        // (e.g. bad endpoint, missing collector). A tracer init failure must
        // never panic the binary — the user still needs the CLI to work.
        match opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(exporter)
            .install_batch(opentelemetry_sdk::runtime::Tokio)
        {
            Ok(tracer_provider) => {
                let otel_layer =
                    tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("travsr"));
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                    .with(otel_layer)
                    .init();
                tracing::info!(otlp_endpoint = %endpoint, "OTLP trace export enabled");
            }
            Err(e) => {
                // Fall back to stderr-only — the CLI must remain functional.
                tracing_subscriber::fmt()
                    .with_writer(std::io::stderr)
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .init();
                tracing::warn!(
                    error = %e,
                    otlp_endpoint = %endpoint,
                    "OTLP exporter init failed — falling back to stderr-only tracing"
                );
            }
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

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => init::run()?,
        Command::Daemon { action } => {
            let cwd = std::env::current_dir()?;
            let repo_root = repo::find_git_root(&cwd)?;
            match action {
                DaemonAction::Start { foreground } => {
                    if foreground {
                        travsr_daemon::Daemon::run(repo_root).await?;
                    } else {
                        // Guard: if a daemon is already responding on the socket,
                        // don't spawn another one. Each `daemon start` call was
                        // otherwise spawning a new background child (visible,
                        // 700 MB each) because the check happened *inside* the
                        // child after it was already running.
                        let sock = repo_root.join(".travsr/daemon.sock");
                        if send_daemon_command(&sock, r#"{"op":"status"}"#).is_ok() {
                            eprintln!("travsr daemon is already running");
                            return Ok(());
                        }
                        // Spawn background child: re-exec with --foreground.
                        let exe = std::env::current_exe().context("finding current exe")?;
                        std::process::Command::new(exe)
                            .args(["daemon", "start", "--foreground"])
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .context("spawning background daemon")?;
                        eprintln!("travsr daemon started in background");
                    }
                }
                DaemonAction::Stop => {
                    let sock = repo_root.join(".travsr/daemon.sock");
                    send_daemon_command(&sock, r#"{"op":"shutdown"}"#)?;
                }
                DaemonAction::Status => {
                    let sock = repo_root.join(".travsr/daemon.sock");
                    let resp = send_daemon_command(&sock, r#"{"op":"status"}"#)?;
                    println!("{resp}");
                }
                DaemonAction::Restart => {
                    let sock = repo_root.join(".travsr/daemon.sock");
                    // Best-effort stop — ignore errors if daemon not running.
                    let _ = send_daemon_command(&sock, r#"{"op":"shutdown"}"#);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let exe = std::env::current_exe().context("finding current exe")?;
                    std::process::Command::new(exe)
                        .args(["daemon", "start", "--foreground"])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .context("respawning daemon")?;
                    eprintln!("travsr daemon restarted in background");
                }
            }
        }
        Command::Mcp {
            stdio: _,
            global,
            db,
        } => {
            if global {
                travsr_mcp::serve_stdio_global()?;
            } else {
                let db_path = if let Some(p) = db {
                    p
                } else {
                    let cwd = std::env::current_dir()?;
                    let repo_root = repo::find_git_root(&cwd)?;
                    repo_root.join(".travsr/graph.db")
                };
                if !db_path.exists() {
                    anyhow::bail!("not initialized — run `travsr init` first");
                }
                travsr_mcp::serve_stdio(&db_path)?;
            }
        }
        Command::Index {
            dir,
            output,
            corpus,
        } => index::run(&dir, &output, &corpus)?,
        Command::Repos => repos::run()?,
        Command::Status => status::run()?,
        Command::Ask { query } => ask::run(&query)?,
        Command::Graph {
            query,
            all,
            depth,
            direction,
            format,
        } => match (all, query.as_deref()) {
            (true, Some(_)) => anyhow::bail!("--all and a query are mutually exclusive"),
            (true, None) => graph::run_all(format)?,
            (false, Some(q)) => graph::run(q, depth, direction, format)?,
            (false, None) => anyhow::bail!("provide a symbol/file query or pass --all"),
        },
        Command::HookRun { from_hook, paths } => {
            let cwd = std::env::current_dir()?;
            let repo_root = repo::find_git_root(&cwd)?;

            // Prefer dispatching to a running daemon — it reindexes async and
            // never blocks the git commit. Fall back to in-process indexing when
            // no daemon is running.
            #[cfg(unix)]
            if travsr_daemon::try_dispatch_to_daemon(&repo_root) {
                return Ok(());
            }

            let mut store = {
                let db_path = repo_root.join(".travsr/graph.db");
                travsr_store::SqliteStore::open(&db_path)?
            };
            let abs_paths: Vec<std::path::PathBuf> = if from_hook {
                travsr_daemon::changed_files_from_git(&repo_root)?
            } else {
                paths.iter().map(|p| repo_root.join(p)).collect()
            };
            travsr_daemon::reindex_files(&abs_paths, &repo_root, &mut store)?;
        }
        Command::Migrate { to } => migrate::run_to(&to)?,
        Command::Serve { port, tenants_dir } => {
            serve::run(port, tenants_dir).await?;
        }
    }
    Ok(())
}

/// Send a JSON command to the running daemon's Unix domain socket and return
/// the trimmed response line. Times out after 5 seconds.
#[cfg(unix)]
fn send_daemon_command(sock: &std::path::Path, msg: &str) -> anyhow::Result<String> {
    use std::io::{BufRead as _, Write as _};
    let mut conn = std::os::unix::net::UnixStream::connect(sock).with_context(|| {
        format!(
            "daemon not running (socket not found at {})",
            sock.display()
        )
    })?;
    conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    writeln!(conn, "{msg}")?;
    let mut resp = String::new();
    std::io::BufReader::new(conn).read_line(&mut resp)?;
    Ok(resp.trim().to_string())
}

#[cfg(not(unix))]
fn send_daemon_command(_sock: &std::path::Path, _msg: &str) -> anyhow::Result<String> {
    anyhow::bail!("daemon control socket not yet supported on Windows")
}
