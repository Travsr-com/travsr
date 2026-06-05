//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod ask;
#[cfg(windows)]
mod autostart;
mod graph;
mod index;
mod init;
mod install;
mod lang;
mod migrate;
mod repo;
mod repos;
mod serve;
mod status;
mod synonym;

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
    Repos {
        /// Remove registry entries whose graph.db no longer exists.
        #[arg(long)]
        prune: bool,
        /// Remove a single repo entry by name.
        #[arg(long, value_name = "NAME")]
        remove: Option<String>,
        /// Emit the registry as JSON ([{name, db_path, exists}]).
        #[arg(long)]
        json: bool,
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
    /// Manage Phase B language tools (semantic analysis).
    Lang {
        #[command(subcommand)]
        action: lang::LangCommand,
    },
    /// Manage per-repo dynamic synonym pairs (RFC-012 A2 F1).
    Synonym {
        #[command(subcommand)]
        action: synonym::SynonymCommand,
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
    // Hidden subcommand: __plugin <lang>
    // Invoked by the daemon's Sidecar::spawn() to run a built-in Phase A plugin
    // over stdin/stdout. Not user-facing — absent from --help output.
    // Must be checked before Clap parses args (Clap would reject `__plugin`).
    if let Some("__plugin") = std::env::args().nth(1).as_deref() {
        let lang = std::env::args().nth(2).unwrap_or_default();
        // Minimal stderr tracing so Phase B failures inside the sidecar (e.g. the
        // LSIF emitter failing to spawn `node`, or an SCIP tool crashing) are
        // observable when the daemon is run with RUST_LOG set. Defaults to error
        // only — no noise in normal operation. We do NOT call the full
        // init_tracing(): that may start the OTLP/Tokio exporter, which the
        // short-lived, stdin/stdout-framed plugin loop must not depend on.
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
            )
            .try_init();
        use travsr_plugin_host::plugins;
        match lang.as_str() {
            "typescript" | "javascript" => {
                travsr_plugin_sdk::run_plugin(plugins::typescript::TypeScriptPlugin)
            }
            "rust" => travsr_plugin_sdk::run_plugin(plugins::rust::RustPlugin),
            "python" => travsr_plugin_sdk::run_plugin(plugins::python::PythonPlugin),
            other => {
                eprintln!("unknown __plugin language: {other}");
                std::process::exit(1);
            }
        }
        return;
    }

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
    // Default to error-only for normal user operation — no internal tracing noise.
    // Set RUST_LOG=info or RUST_LOG=debug to see internals during development.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));

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
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
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
                        // Guard: if a daemon is already responding on the transport,
                        // don't spawn another one. Each `daemon start` call was
                        // otherwise spawning a new background child (visible,
                        // 700 MB each) because the check happened *inside* the
                        // child after it was already running.
                        if daemon_is_running(&repo_root, 3, 300) {
                            eprintln!("travsr daemon is already running");
                            return Ok(());
                        }
                        // Spawn background child: re-exec with --foreground.
                        let exe = std::env::current_exe().context("finding current exe")?;
                        std::process::Command::new(&exe)
                            .args(["daemon", "start", "--foreground"])
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .context("spawning background daemon")?;
                        eprintln!("travsr daemon started in background");
                        // Windows: register a Task Scheduler ONLOGON task so the
                        // daemon auto-starts after reboot. Non-fatal — the daemon
                        // is already running; auto-start just won't persist.
                        #[cfg(windows)]
                        if let Err(e) = autostart::register(&exe, &repo_root) {
                            eprintln!("travsr: warning: could not register auto-start task: {e}");
                        }
                    }
                }
                DaemonAction::Stop => {
                    send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Shutdown)?;
                    // Windows: remove the auto-start task so the daemon stays
                    // stopped after the user logs out and back in.
                    #[cfg(windows)]
                    if let Err(e) = autostart::unregister(&repo_root) {
                        eprintln!("travsr: warning: could not remove auto-start task: {e}");
                    }
                }
                DaemonAction::Status => {
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Status) {
                        Ok(_) => {
                            let transport = if cfg!(windows) { "named_pipe" } else { "unix" };
                            println!("running [transport={transport}]");
                        }
                        Err(_) => println!("not running"),
                    }
                }
                DaemonAction::Restart => {
                    // Best-effort stop — ignore errors if daemon not running.
                    let _ = send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Shutdown);
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
        Command::Repos {
            prune,
            remove,
            json,
        } => repos::run(prune, remove.as_deref(), json)?,
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
            // no daemon is running (or on Windows before RFC-013 lands).
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
        Command::Lang { action } => lang::run(action)?,
        Command::Synonym { action } => synonym::run(action)?,
    }
    Ok(())
}

/// Connect to the daemon's control transport for `repo_root`.
///
/// Dispatches to the platform-appropriate transport:
/// Unix → `UnixTransport` (domain socket), Windows → `NamedPipeTransport`.
fn send_daemon_command(
    repo_root: &std::path::Path,
    msg: &travsr_ipc::ControlMessage,
) -> anyhow::Result<travsr_ipc::ControlResponse> {
    let addr = travsr_ipc::ControlAddr::for_repo(repo_root);

    #[cfg(unix)]
    {
        let travsr_dir = repo_root.join(".travsr");
        let mut t = travsr_ipc::unix::UnixTransport::connect(&addr, &travsr_dir)?;
        travsr_ipc::ControlTransport::send_request(&mut t, msg)
    }

    #[cfg(windows)]
    {
        let mut t = travsr_ipc::windows::NamedPipeTransport::connect(&addr)?;
        travsr_ipc::ControlTransport::send_request(&mut t, msg)
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (addr, msg);
        anyhow::bail!("daemon control socket not supported on this platform")
    }
}

/// Retry pinging the daemon transport up to `attempts` times with `delay_ms` between tries.
///
/// launchd `KeepAlive: true` restarts the daemon after `daemon stop`, so the
/// transport may briefly be unavailable during restart. Without retries, a
/// `daemon start` call immediately after `daemon stop` could incorrectly spawn
/// a second daemon.
fn daemon_is_running(repo_root: &std::path::Path, attempts: u32, delay_ms: u64) -> bool {
    for i in 0..attempts {
        if i > 0 {
            // Blocking sleep is safe: no concurrent async tasks exist at daemon-start time.
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        if send_daemon_command(repo_root, &travsr_ipc::ControlMessage::Status).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_is_running_returns_false_when_no_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        // No daemon running in tmp — transport connect should fail immediately.
        // Attempts=1, delay=0 — must not block.
        assert!(!daemon_is_running(tmp.path(), 1, 0));
    }
}
