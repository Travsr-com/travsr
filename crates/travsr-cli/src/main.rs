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
        /// Address to bind on. Defaults to loopback: this server speaks
        /// plaintext HTTP with bearer-token auth, so exposing it beyond this
        /// machine should be deliberate. Pass `0.0.0.0` when a TLS terminator
        /// sits in front (#410).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
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
    /// Print daemon log entries. Reads the file directly, so it works after a
    /// crash and does not need a running daemon.
    Logs {
        /// Stream new lines as they are written, following rotation.
        #[arg(long, short = 'f', default_value_t = false)]
        follow: bool,
        /// Lines to show from the end of the log. 0 prints the whole file.
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// Show only lines tagged with this repo. Useful against a log that
        /// serves several repos.
        #[arg(long)]
        repo: Option<String>,
    },
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
                        // #348: open the tail *before* spawning and park it at
                        // the end. Today's log file usually already exists from
                        // an earlier session, so a tail opened afterwards would
                        // replay history as if it were this startup.
                        let mut relay = travsr_daemon::logfile::LogTail::new(
                            &travsr_daemon::logfile::log_dir(&repo_root),
                        );
                        relay.seek_to_end();
                        let started = std::time::Instant::now();

                        match daemon_client::spawn_background_daemon(&repo_root, &exe) {
                            daemon_client::SpawnOutcome::AlreadyRunning => {
                                eprintln!("travsr daemon is already running");
                                return Ok(());
                            }
                            daemon_client::SpawnOutcome::Started
                            | daemon_client::SpawnOutcome::Starting => {
                                relay_daemon_startup(&repo_root, relay, started);
                            }
                            daemon_client::SpawnOutcome::Failed => {
                                match daemon_start_error(&repo_root) {
                                    Some(r) => {
                                        eprintln!("travsr daemon failed to start: {r}")
                                    }
                                    None => eprintln!(
                                        "travsr daemon failed to start — see `travsr daemon logs`"
                                    ),
                                }
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
                    // #541: capture the PID first — a clean shutdown removes the
                    // lock file, so this is the only chance to learn who to
                    // check. Everything below turns "the request was accepted"
                    // into "the process is gone", which is what callers of
                    // `daemon stop` actually need the exit status to mean: a
                    // stale daemon left alive keeps serving queries from an old
                    // binary image and silently invalidates any live check made
                    // against it.
                    let pid_before = daemon_lock_pid(&repo_root);

                    // M2/L1: if the daemon is not running, tell the user clearly
                    // and exit 0 — it's not an error to stop something already stopped.
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::Shutdown) {
                        Ok(_) => match pid_before {
                            Some(pid) if !wait_for_exit(pid, STOP_EXIT_TIMEOUT) => {
                                anyhow::bail!(
                                    "travsr daemon acknowledged the stop request but process \
                                     {pid} is still running after {}s.\n  \
                                     Any `travsr` query may still be answered by it, from the \
                                     binary image it started with.\n  \
                                     Check with `travsr daemon status`, or stop it directly \
                                     with `kill {pid}`.",
                                    STOP_EXIT_TIMEOUT.as_secs()
                                );
                            }
                            // No lock file to check against: report what was
                            // actually established rather than overclaiming.
                            //
                            // This is the one branch where the exit status
                            // means "accepted", not "confirmed gone" — there is
                            // no PID to verify against, so there is nothing to
                            // verify. The wording carries that distinction
                            // because the exit code cannot.
                            None => eprintln!("travsr daemon stop acknowledged"),
                            Some(_) => eprintln!("travsr daemon stopped"),
                        },
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("No such file")
                                || msg.contains("Connection refused")
                                || msg.contains("os error 2")
                                || msg.contains("os error 111")
                                || msg.contains("os error 61")
                            {
                                eprintln!("travsr daemon is not running");
                            } else if let Some(pid) = pid_before.filter(|p| pid_is_alive(*p)) {
                                // #541: the original report. The transport now
                                // retries a would-block, so reaching here means
                                // something worse than a transient stall — but
                                // the daemon is demonstrably still up, and that
                                // is the part the user has to act on, not the
                                // errno.
                                return Err(e.context(format!(
                                    "the daemon (process {pid}) is still running and was NOT \
                                     stopped; stop it directly with `kill {pid}` if it stays wedged"
                                )));
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
                                match daemon_start_error(&repo_root) {
                                    Some(r) => {
                                        println!("daemon: not running (last start failed: {r})")
                                    }
                                    None => println!("daemon: not running"),
                                }
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
                        daemon_client::SpawnOutcome::Failed => match daemon_start_error(&repo_root)
                        {
                            Some(r) => eprintln!("travsr daemon failed to restart: {r}"),
                            None => eprintln!(
                                "travsr daemon failed to restart — see `travsr daemon logs`"
                            ),
                        },
                        _ => eprintln!("travsr daemon restarted in background"),
                    }
                }
                DaemonAction::Logs {
                    follow,
                    lines,
                    repo,
                } => {
                    daemon_logs(&repo_root, follow, lines, repo.as_deref())?;
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
        Command::Serve {
            host,
            port,
            tenants_dir,
        } => {
            serve::run(host, port, tenants_dir).await?;
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

/// Read the daemon's startup-error breadcrumb (`.travsr/daemon-start.err`),
/// written when a detached daemon crashes during startup (e.g. a control-socket
/// bind failure). `None` when the file is absent or empty. Lets `daemon
/// start`/`status`/`restart` surface a background failure that would otherwise
/// be silent (travsr #592).
/// How long the parent waits for a spawned daemon to answer before giving up.
///
/// Covers a slow machine doing a first-run Phase A on a large tree. The daemon
/// is not killed on expiry — it is still starting, and the message says so.
const STARTUP_RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// #348: relay the daemon's own log to the terminal until it can answer.
///
/// `travsr daemon start` used to print one line in under 10ms and return the
/// shell prompt, which said nothing about whether the daemon connected, began
/// indexing, or died. This blocks for as long as startup takes and shows what
/// the daemon is actually doing.
///
/// Exits when the control socket answers, which is the point the daemon can
/// serve MCP queries. Phase B may still be running; it is background by design
/// and waiting for it would misrepresent readiness.
///
/// Startup *failures* are deliberately not diagnosed from this stream.
/// `.travsr/daemon-start.err` already carries the reason (#592) and
/// `daemon_start_error` already reports it; duplicating that here would give
/// two sources for one answer that could disagree.
///
/// `started` is captured by the caller *before* spawning, because
/// `spawn_background_daemon` itself waits on the child. Starting the clock here
/// measured only the relay and reported "ready in 0.0s" for a startup that took
/// most of a second.
fn relay_daemon_startup(
    repo_root: &std::path::Path,
    mut relay: travsr_daemon::logfile::LogTail,
    started: std::time::Instant,
) {
    let deadline = started + STARTUP_RELAY_TIMEOUT;
    eprintln!("[travsr] starting…");

    loop {
        if let Ok(lines) = relay.poll() {
            for line in lines {
                eprintln!("[travsr] {line}");
            }
        }

        // One attempt, no internal sleep: this loop already paces itself, and a
        // nested delay would double the interval and blur the elapsed figure.
        if daemon_is_running(repo_root, 1, 0) {
            eprintln!("[travsr] ready in {:.1}s", started.elapsed().as_secs_f32());
            return;
        }
        if std::time::Instant::now() >= deadline {
            // Not an error: the daemon may simply still be scanning. Say what
            // is known rather than claiming a failure that may not have
            // happened.
            eprintln!(
                "[travsr] still starting after {}s — follow it with `travsr daemon logs --follow`",
                STARTUP_RELAY_TIMEOUT.as_secs()
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Whether a log line belongs to `repo`.
///
/// The daemon tags repo-scoped work with a `repo` span field, which the
/// formatter renders as `repo="<name>"`. Matching on that rendering rather
/// than parsing the line keeps this independent of the rest of the format.
fn line_is_for_repo(line: &str, repo: &str) -> bool {
    line.contains(&format!("repo=\"{repo}\"")) || line.contains(&format!("repo={repo}"))
}

/// `travsr daemon logs` — print, and optionally follow, the daemon log.
///
/// Reads the file rather than asking the daemon, so it still works after a
/// crash, which is when it is most wanted. Output carries no ANSI: these lines
/// get piped into `grep` far more often than they get read directly.
fn daemon_logs(
    repo_root: &std::path::Path,
    follow: bool,
    lines: usize,
    repo: Option<&str>,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let dir = travsr_daemon::logfile::log_dir(repo_root);
    let mut tail = travsr_daemon::logfile::LogTail::new(&dir);

    if tail.path().is_none() {
        // Name the directory, not a filename: the log is dated, so telling the
        // user to look for `daemon.log` would send them after a file that is
        // never created.
        eprintln!(
            "no daemon log in {} yet — run `travsr daemon start`",
            dir.display()
        );
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let backfill = tail.backfill(lines)?;
    for line in backfill.lines() {
        if repo.is_none_or(|r| line_is_for_repo(line, r)) {
            writeln!(out, "{line}")?;
        }
    }
    out.flush()?;

    if !follow {
        return Ok(());
    }

    // Only lines from here on; the backfill above already covered history.
    tail.seek_to_end();
    loop {
        for line in tail.poll()? {
            if repo.is_none_or(|r| line_is_for_repo(&line, r)) {
                writeln!(out, "{line}")?;
            }
        }
        // Flush every tick: a follower that buffers is indistinguishable from a
        // daemon that has stopped logging.
        out.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

fn daemon_start_error(repo_root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(repo_root.join(".travsr").join("daemon-start.err"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// PID recorded in `.travsr/daemon.lock`, if the file exists and parses.
///
/// #541: read *before* sending `Shutdown`, because a daemon that exits cleanly
/// takes the lock file with it — after the fact there is nothing left to check
/// liveness against.
fn daemon_lock_pid(repo_root: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(repo_root.join(".travsr/daemon.lock"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Wait up to `timeout` for `pid` to disappear. Returns `true` if it did.
///
/// #541: `Shutdown` being acknowledged only means the daemon *received* the
/// request. The process tears down a watcher, a scheduler and a store after
/// that, so "stopped" is a claim that has to be checked rather than assumed.
///
/// Races with PID reuse: if the daemon exits and the OS recycles its number
/// onto an unrelated process inside the window, this reports "still running"
/// and `daemon stop` fails where it should have succeeded. The failure
/// direction is the safe one — it tells the user to check rather than claiming
/// a stop that did not happen — and `pid_is_alive` is the convention the rest
/// of this file already uses for liveness. A stronger signal exists (a clean
/// exit releases the `flock` on `daemon.lock`, so re-acquirability
/// distinguishes "my daemon" from "some new process with its number"), and is
/// the right upgrade if this ever misfires in practice.
fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let cutoff = std::time::Instant::now() + timeout;
    loop {
        if !pid_is_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= cutoff {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// How long `daemon stop` waits for the process to actually exit before
/// reporting that it is still running (#541). Generous enough for a teardown
/// that has to flush a store, short enough to stay scriptable.
const STOP_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

    /// #541: `daemon stop` reports success only once the process is actually
    /// gone, so the wait has to distinguish "exited" from "still there" rather
    /// than trusting the Shutdown acknowledgement.
    #[test]
    fn wait_for_exit_detects_an_exited_process() {
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
            wait_for_exit(pid, std::time::Duration::from_secs(5)),
            "an exited process must be observed as gone"
        );
    }

    #[test]
    fn wait_for_exit_times_out_on_a_process_that_stays_up() {
        // The #541 case: Shutdown was acknowledged but the daemon never went
        // away. This must be reported, not silently treated as success.
        assert!(
            !wait_for_exit(std::process::id(), std::time::Duration::from_millis(150)),
            "a process that is still running must not be reported as exited"
        );
    }

    /// The PID has to be read before the stop request, since a clean shutdown
    /// deletes the lock file and leaves nothing to check liveness against.
    #[test]
    fn daemon_lock_pid_reads_the_lock_file_and_tolerates_its_absence() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(daemon_lock_pid(dir.path()), None, "no .travsr yet");

        std::fs::create_dir_all(dir.path().join(".travsr")).unwrap();
        std::fs::write(dir.path().join(".travsr/daemon.lock"), "4242\n").unwrap();
        assert_eq!(daemon_lock_pid(dir.path()), Some(4242));

        std::fs::write(dir.path().join(".travsr/daemon.lock"), "not-a-pid").unwrap();
        assert_eq!(
            daemon_lock_pid(dir.path()),
            None,
            "a corrupt lock file must not panic or yield a bogus pid"
        );
    }
}
