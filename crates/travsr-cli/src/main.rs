//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod ask;
#[cfg(windows)]
mod autostart;
mod config;
mod daemon_client;
mod embed;
mod explain;
mod fsck;
mod graph;
mod index;
mod init;
mod install;
mod lang;
mod logo;
mod pattern;
mod progress;
mod references;
mod repo;
mod repos;
mod rerank;
mod serve;
mod status;
mod synonym;

use anyhow::{Context as _, Result};
use clap::{CommandFactory as _, FromArgMatches as _, Parser, Subcommand};

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
    Init {
        /// Suppress progress output and post-index tips.
        #[arg(long, short)]
        quiet: bool,
        /// Emit machine-readable JSON (summary on stdout, progress on stderr).
        #[arg(long)]
        json: bool,
        /// Number of parallel parse workers (default: available CPU cores).
        #[arg(long, value_name = "N")]
        jobs: Option<usize>,
        /// Run semantic Phase B (call edges) synchronously before returning.
        /// By default Phase B runs in the background via the daemon.
        /// Use this in CI or scripts that query call edges immediately after init.
        #[arg(long)]
        semantic: bool,
        /// Allow rust-analyzer to run UNCONFINED when the OS sandbox (bubblewrap
        /// on Linux, sandbox-exec on macOS) is unavailable. Without this flag,
        /// the rust-analyzer LSIF pass is skipped entirely on sandbox-less hosts
        /// and Rust semantic edges degrade to tree-sitter structural edges.
        ///
        /// Only set this when you fully trust the repository being indexed.
        /// This flag cannot be set by repository contents (.env, Cargo.toml,
        /// tsconfig, etc.) — it must be an explicit, per-invocation decision.
        #[arg(long)]
        allow_unsandboxed_lsif: bool,
    },
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
    /// Ask a natural-language question about the codebase (graph-grounded
    /// retrieval). Also accepts a bare symbol name.
    Ask {
        /// Natural-language question, or a symbol name, to retrieve context for.
        query: String,
        /// Output format: table (default) or json.
        #[arg(long, value_enum, default_value = "table")]
        format: ask::OutputFormat,
    },
    /// #478 RFC-023 §6.1: diagnostic seed-building trace for one query/symbol
    /// pair — per-token IDF, per-leg match, every gate + threshold, and final
    /// disposition (live vs an FTS-only counterfactual). The instrument for
    /// tuning retrieval thresholds; not part of the normal retrieval path.
    Explain {
        /// Natural-language question, or a symbol name, exactly as you would
        /// pass it to `travsr ask`.
        query: String,
        /// The symbol to explain (bare name or full signature).
        symbol: String,
        /// Output format: text (default) or json.
        #[arg(long, value_enum, default_value = "text")]
        format: explain::OutputFormat,
    },
    /// Enumerate every use site (path:line) of a symbol across the repo.
    #[command(alias = "refs")]
    References {
        /// Symbol to enumerate references of (bare name or full signature).
        symbol: String,
        /// Optional file-path hint to disambiguate overloaded names.
        #[arg(long)]
        path: Option<String>,
        /// Output format: text (default) or json.
        #[arg(long, value_enum, default_value = "text")]
        format: references::OutputFormat,
    },
    /// Graph-scoped textual search (git grep) over a bounded file set.
    Pattern {
        /// POSIX ERE pattern to search for (use --fixed for a literal string).
        pattern: String,
        /// Optional scope: a path prefix, or files-importing(<symbol>).
        #[arg(long)]
        scope: Option<String>,
        /// Treat pattern as a literal string instead of a regular expression.
        #[arg(long)]
        fixed: bool,
        /// Output format: text (default) or json.
        #[arg(long, value_enum, default_value = "text")]
        format: pattern::OutputFormat,
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
        /// Edge-follow mode: semantic prefers call/override edges over imports (ignored with --all).
        #[arg(long, default_value = "semantic")]
        edges: graph::EdgeMode,
        /// Include third-party and anonymous-local nodes in traversal output.
        #[arg(long)]
        include_noise: bool,
        /// Cap output to roughly this many tokens (0 = unlimited). Truncation
        /// keeps the closest nodes first and reports how many were cut.
        #[arg(long, default_value = "4096")]
        budget: usize,
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
    /// Manage per-repo synonym pairs for richer semantic search.
    Synonym {
        #[command(subcommand)]
        action: synonym::SynonymCommand,
    },
    /// Manage the RFC-021 cross-encoder reranker model.
    Rerank {
        #[command(subcommand)]
        action: rerank::RerankCommand,
    },
    /// Manage embedding backends for semantic code search.
    Embed {
        #[command(subcommand)]
        action: embed::EmbedCommand,
    },
    /// Inspect and set layered configuration (global + per-repo).
    Config {
        #[command(subcommand)]
        action: config::ConfigCommand,
    },
    /// Check graph integrity; optionally repair ghost nodes and orphan edges.
    Fsck {
        /// Delete ghost nodes and sweep orphan edges (default: report only).
        #[arg(long)]
        fix: bool,
        /// Emit results as JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
        /// Override the mass-delete circuit breaker (use with care).
        #[arg(long)]
        force: bool,
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
    /// Pause background embed reindexing and cancel any in-flight reindex
    /// (graceful — partial embeddings are preserved). Pause lasts until
    /// `resume-embed` or a daemon restart.
    StopEmbed,
    /// Resume background embed reindexing paused by `stop-embed`.
    ResumeEmbed,
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
    // Brand header for `--help`: the real logo as truecolor half-block art on a
    // color terminal, else the geometric motif. Both are pure SGR, which clap's
    // before_help preserves.
    let matches = Cli::command().before_help(logo::banner()).get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    let is_daemon = matches!(&cli.command, Command::Daemon { .. });
    if !is_daemon {
        init_tracing();
    }

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
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_otlp::WithExportConfig as _;
        use opentelemetry_sdk::trace::SdkTracerProvider;
        use tracing_subscriber::prelude::*;

        let endpoint = std::env::var("TRAVSR_OTLP_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:4317".to_string());

        // Gracefully degrade to stderr-only if the OTLP pipeline setup fails
        // (e.g. bad endpoint, missing collector). A tracer init failure must
        // never panic the binary — the user still needs the CLI to work.
        match opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&endpoint)
            .build()
            .map(|exporter| {
                SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .build()
            }) {
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
        Command::Init {
            quiet,
            json,
            jobs,
            semantic,
            allow_unsandboxed_lsif,
        } => init::run(quiet, json, jobs, semantic, allow_unsandboxed_lsif)?,
        Command::Daemon { action } => {
            let cwd = std::env::current_dir()?;
            // The daemon indexes/watches the resolved root: bind it to the
            // worktree we are in, never the main worktree (issue #586).
            let repo_root = repo::find_git_root_for_write(&cwd)?;
            match action {
                DaemonAction::Start { foreground } => {
                    if foreground {
                        travsr_daemon::Daemon::run(repo_root, foreground).await?;
                    } else {
                        // Race-free guard: consult the flock the daemon itself
                        // holds (not a socket probe), and only spawn a child when
                        // no daemon owns the lock — so we never fork a doomed
                        // ~700 MB process that pays full startup then dies.
                        let exe = std::env::current_exe().context("finding current exe")?;
                        match daemon_client::spawn_background_daemon(&repo_root, &exe) {
                            daemon_client::SpawnOutcome::AlreadyRunning => {
                                eprintln!("travsr daemon is already running");
                                return Ok(());
                            }
                            daemon_client::SpawnOutcome::Started => {
                                eprintln!("travsr daemon started in background");
                            }
                            daemon_client::SpawnOutcome::Failed => {
                                eprintln!("travsr daemon failed to start (see .travsr/daemon.log)");
                                return Ok(());
                            }
                        }
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
                    // M2/L1: if the daemon is not running, tell the user clearly
                    // and exit 0 — it's not an error to stop something already stopped.
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Shutdown) {
                        Ok(_) => {
                            eprintln!("travsr daemon stopped");
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("No such file")
                                || msg.contains("Connection refused")
                                || msg.contains("os error 2")
                                || msg.contains("os error 111")
                                || msg.contains("os error 61")
                            {
                                eprintln!("travsr daemon is not running");
                            } else {
                                return Err(e);
                            }
                        }
                    }
                    // Windows: remove the auto-start task so the daemon stays
                    // stopped after the user logs out and back in.
                    #[cfg(windows)]
                    if let Err(e) = autostart::unregister(&repo_root) {
                        eprintln!("travsr: warning: could not remove auto-start task: {e}");
                    }
                }
                DaemonAction::Status => {
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Status) {
                        Ok(resp) => {
                            let transport = if cfg!(windows) { "named_pipe" } else { "unix" };
                            println!("daemon: running [transport={transport}]");
                            if let Some(msg) = resp.message {
                                println!();
                                println!("{msg}");
                            }
                        }
                        Err(_) => {
                            // Socket not ready yet — check lock file PID.
                            // On a large repo the watcher initial scan can take
                            // 10-30 s; the daemon is alive but hasn't bound its
                            // socket yet.
                            let lock_path = repo_root.join(".travsr/daemon.lock");
                            let starting = std::fs::read_to_string(&lock_path)
                                .ok()
                                .and_then(|s| s.trim().parse::<u32>().ok())
                                .map(pid_is_alive)
                                .unwrap_or(false);
                            if starting {
                                println!(
                                    "daemon: starting (scanning file tree — socket not ready yet)"
                                );
                            } else {
                                println!("daemon: not running");
                            }
                        }
                    }
                }
                DaemonAction::Restart => {
                    // Best-effort stop — ignore errors if daemon not running.
                    let _ = send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Shutdown);
                    // Wait for the old daemon to actually release its lock (≤2 s)
                    // instead of a fixed sleep, so the new one never races the old
                    // one's shutdown and fails to acquire the lock.
                    for _ in 0..40 {
                        if !daemon_client::daemon_lock_held(&repo_root) {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    let exe = std::env::current_exe().context("finding current exe")?;
                    match daemon_client::spawn_background_daemon(&repo_root, &exe) {
                        daemon_client::SpawnOutcome::Failed => {
                            eprintln!("travsr daemon failed to restart (see .travsr/daemon.log)");
                        }
                        _ => eprintln!("travsr daemon restarted in background"),
                    }
                }
                DaemonAction::StopEmbed => {
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::StopEmbed) {
                        Ok(resp) => eprintln!(
                            "{}",
                            resp.message
                                .unwrap_or_else(|| "embed auto-reindex paused".into())
                        ),
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("No such file")
                                || msg.contains("Connection refused")
                                || msg.contains("os error 2")
                                || msg.contains("os error 111")
                                || msg.contains("os error 61")
                            {
                                eprintln!("travsr daemon is not running");
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
                DaemonAction::ResumeEmbed => {
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::ResumeEmbed)
                    {
                        Ok(resp) => eprintln!(
                            "{}",
                            resp.message
                                .unwrap_or_else(|| "embed auto-reindex resumed".into())
                        ),
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("No such file")
                                || msg.contains("Connection refused")
                                || msg.contains("os error 2")
                                || msg.contains("os error 111")
                                || msg.contains("os error 61")
                            {
                                eprintln!("travsr daemon is not running");
                            } else {
                                return Err(e);
                            }
                        }
                    }
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
                    // H2: MCP clients read stdout as a JSON-RPC stream. An abrupt
                    // exit with no output causes an opaque EOF error on the client.
                    // Write a notification-style error first so the client can
                    // show a human-readable message before disconnecting.
                    let err_msg = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/message",
                        "params": {
                            "level": "error",
                            "data": "travsr: not initialized — run `travsr init` first, then retry"
                        }
                    });
                    println!("{err_msg}");
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
        Command::Ask { query, format } => ask::run(&query, format)?,
        Command::Explain {
            query,
            symbol,
            format,
        } => explain::run(&query, &symbol, format)?,
        Command::References {
            symbol,
            path,
            format,
        } => references::run(&symbol, path, format)?,
        Command::Pattern {
            pattern,
            scope,
            fixed,
            format,
        } => pattern::run(&pattern, scope, fixed, format)?,
        Command::Graph {
            query,
            all,
            depth,
            direction,
            format,
            edges,
            include_noise,
            budget,
        } => match (all, query.as_deref()) {
            (true, Some(_)) => anyhow::bail!("--all and a query are mutually exclusive"),
            (true, None) => graph::run_all(format, budget)?,
            (false, Some(q)) => {
                graph::run(q, depth, direction, format, edges, include_noise, budget)?
            }
            (false, None) => anyhow::bail!("provide a symbol/file query or pass --all"),
        },
        Command::HookRun { from_hook, paths } => {
            let cwd = std::env::current_dir()?;
            // A commit reindexes the worktree it happened in, never the main
            // worktree (issue #586).
            let repo_root = repo::find_git_root_for_write(&cwd)?;

            // travsr git hooks live in the shared `$GIT_COMMON_DIR/hooks` and
            // fire for every linked worktree. A worktree that was never
            // `travsr init`-ed has no index of its own; skip rather than let
            // SqliteStore::open create a stray empty graph.db for a tree the
            // user never opted into (issue #586).
            if !repo_root.join(".travsr/graph.db").exists() {
                return Ok(());
            }

            // Prefer dispatching to a running daemon — it reindexes async and
            // never blocks the git commit. Fall back to in-process indexing when
            // no daemon is running.
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
            let dirty = travsr_daemon::reindex_files(&abs_paths, &repo_root, &mut store)?;
            if !dirty.is_empty() {
                // No daemon running: Tier-0 callers cannot be re-enqueued.
                // Phase B on the next commit will re-resolve cross-file edges.
                tracing::debug!(
                    callers = dirty.len(),
                    "hook-run (no daemon): {} Tier-0 caller(s) deferred to next Phase B",
                    dirty.len()
                );
            }
        }
        Command::Serve { port, tenants_dir } => {
            serve::run(port, tenants_dir).await?;
        }
        Command::Lang { action } => lang::run(action)?,
        Command::Synonym { action } => synonym::run(action)?,
        Command::Rerank { action } => rerank::run(action)?,
        Command::Embed { action } => embed::run(action)?,
        Command::Config { action } => config::run(action)?,
        Command::Fsck { fix, json, force } => fsck::run(fix, json, force)?,
    }
    Ok(())
}

/// Check whether a process with the given PID is currently alive.
/// Uses `kill -0` on Unix and `tasklist` on Windows (no signal sent).
fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // /proc/<pid> exists on Linux; on macOS use kill -0 via Command.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // #503: the previous hardcoded `false` broke both fallbacks that
        // exist for the window before the control transport binds — `daemon
        // status` reported "not running" against a live daemon still
        // scanning, and daemon_is_running callers spawned a doomed duplicate.
        // tasklist CSV output quotes the PID as a field only when the
        // process exists; the localized "no tasks" INFO line never does.
        // (forbid(unsafe_code) rules out OpenProcess here; spawning a probe
        // process matches the Unix `kill -0` pattern above.)
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Connect to the daemon's control transport for `repo_root`.
/// Moved to `daemon_client` (#318 O1) — kept as a local alias for callers here.
fn send_daemon_command(
    repo_root: &std::path::Path,
    msg: &travsr_ipc::ControlMessage,
) -> anyhow::Result<travsr_ipc::ControlResponse> {
    daemon_client::send_daemon_command(repo_root, msg)
}

/// Retry pinging the daemon transport up to `attempts` times with `delay_ms` between tries.
///
/// Also checks the lock file PID so that a daemon that is alive but still
/// doing its initial file-tree scan (socket not bound yet) is not mistaken
/// for "not running" — which would cause a second daemon to be spawned and
/// crash immediately with "another daemon already running".
pub(crate) fn daemon_is_running(repo_root: &std::path::Path, attempts: u32, delay_ms: u64) -> bool {
    for i in 0..attempts {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        if send_daemon_command(repo_root, &travsr_ipc::ControlMessage::Status).is_ok() {
            return true;
        }
    }
    // Socket not ready — fall back to lock-file PID check.
    let lock_path = repo_root.join(".travsr/daemon.lock");
    std::fs::read_to_string(&lock_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(pid_is_alive)
        .unwrap_or(false)
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

    /// #503 regression: pid_is_alive must report real liveness on every
    /// platform. The Windows branch was hardcoded `false`, which made the
    /// startup race guard treat a live-but-still-scanning daemon as absent.
    #[test]
    fn pid_is_alive_reports_running_and_exited_processes() {
        assert!(
            pid_is_alive(std::process::id()),
            "the current process must be reported alive"
        );

        let mut child = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        } else {
            std::process::Command::new("true")
        }
        .spawn()
        .expect("spawn short-lived child");
        let pid = child.id();
        child.wait().expect("wait for child");
        assert!(
            !pid_is_alive(pid),
            "an exited, reaped child must be reported dead"
        );
    }
}
