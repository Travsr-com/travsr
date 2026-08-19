//! `travsr` — the command-line entrypoint.

#![forbid(unsafe_code)]

mod ask;
#[cfg(windows)]
mod autostart;
mod config;
mod connect;
mod daemon_client;
mod embed;
mod explain;
mod fsck;
mod git_bounded;
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
mod sidecar_health;
mod status;
mod synonym;

use anyhow::{Context as _, Result};
use clap::{CommandFactory as _, FromArgMatches as _, Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Resolve the reported version: an injected build id wins, the crate version
/// is the fallback.
///
/// Split out as a real function rather than inlined into the `const` below so a
/// test can exercise this exact code with both inputs. A test that reimplements
/// the match would pass even if these arms were swapped.
const fn resolve_version(
    injected: Option<&'static str>,
    crate_version: &'static str,
) -> &'static str {
    match injected {
        Some(v) => v,
        None => crate_version,
    }
}

/// The build identity reported by `--version` and by the daemon's `version=`
/// telemetry field.
///
/// The shipped `v1.0.0-beta.1` binary reported a bare `1.0.0`, identical to what
/// the eventual stable `1.0.0` reports, so a tester's version string could not be
/// attributed to a build.
///
/// It deliberately does **not** carry the prerelease suffix. `release.yml`'s
/// `promote` job reuses the source channel's signed artifacts byte for byte and
/// never rebuilds, so `beta.1 -> rc.1 -> stable` all ship the same binary. Baking
/// in `1.0.0-beta.1` would make every promoted stable release report itself as a
/// beta forever, which is a worse lie than the ambiguity it replaced.
///
/// Instead the release job injects `<tag base>+<short commit>`. The tag base is
/// promotion-stable (`v1.0.0-beta.1` and `v1.0.0` share the base `1.0.0`), and
/// the commit identifies the build itself: `beta.1` and `beta.2` differ because
/// they are different commits, while a promoted stable matches the beta it came
/// from because it genuinely is the same bits. Unset on local builds, which fall
/// back to the crate version.
const RELEASE_VERSION: &str =
    resolve_version(option_env!("TRAVSR_BUILD_ID"), env!("CARGO_PKG_VERSION"));

#[derive(Debug, Parser)]
#[command(
    name = "travsr",
    version = RELEASE_VERSION,
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
        /// Build full cross-file analysis (call edges) synchronously before returning.
        /// By default it runs in the background via the daemon.
        /// Use this in CI or scripts that query call edges immediately after init.
        #[arg(long)]
        semantic: bool,
        /// Force a full rebuild, bypassing the incremental "up to date" skip.
        /// Re-parses every file even when nothing changed on disk — use it after
        /// changing a flag that affects semantic output (e.g. --allow-unsandboxed-lsif)
        /// which the per-file change detection does not otherwise pick up.
        #[arg(long, visible_alias = "rebuild")]
        force: bool,
        /// Allow rust-analyzer to run without OS sandboxing (bubblewrap on Linux,
        /// sandbox-exec on macOS) when no sandbox is available. Without this flag,
        /// Rust's full cross-file analysis is skipped on hosts that have no
        /// sandbox, and Rust falls back to structural analysis only.
        ///
        /// Only set this when you fully trust the repository being indexed.
        /// This flag cannot be set by repository contents (.env, Cargo.toml,
        /// tsconfig, etc.) — it must be an explicit, per-invocation decision.
        #[arg(long, visible_alias = "allow-unsandboxed")]
        allow_unsandboxed_lsif: bool,
        /// Skip auto-detecting AI coding tools and wiring them to Travsr.
        #[arg(long)]
        no_connect: bool,
    },
    /// Detect AI coding tools and wire them to the Travsr MCP server + rules.
    Connect {
        /// Only configure this tool id (run `travsr connect --print` to see the ids).
        #[arg(long)]
        tool: Option<String>,
        /// Show what would change without writing anything.
        #[arg(long)]
        print: bool,
        /// Remove previously generated Travsr config.
        #[arg(long)]
        remove: bool,
        /// Do not git-ignore generated files (opt in to committing them).
        #[arg(long)]
        commit: bool,
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
    /// Show why `travsr ask` ranked (or skipped) results for a query: which
    /// terms matched, which relevance thresholds passed or failed, and the final
    /// decision. A diagnostic for tuning search; not part of normal use.
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
    // UX-008: accept the MCP tool name (`find_references`) so users moving
    // between the docs/MCP surface and the CLI find the same command. Both the
    // underscore (MCP) and hyphen (CLI-convention) spellings resolve.
    #[command(
        visible_alias = "refs",
        alias = "find_references",
        alias = "find-references"
    )]
    References {
        /// Symbol to enumerate references of (bare name or full signature).
        symbol: String,
        /// Optional path hint to scope an overloaded name to a file or
        /// directory: a filename, relative path, directory, or path fragment
        /// (e.g. ppr.rs, src/ppr.rs, crates/travsr-retrieval, retrieval).
        #[arg(long)]
        path: Option<String>,
        /// Output format: text (default) or json.
        #[arg(long, value_enum, default_value = "text")]
        format: references::OutputFormat,
    },
    /// Graph-scoped textual search (git grep) over a bounded file set.
    // UX-008: accept the MCP tool name `find_pattern`.
    #[command(visible_alias = "find_pattern", alias = "find-pattern")]
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
    // UX-008: the graph command is the CLI home of the MCP graph-traversal tools
    // (`get_dependencies`, `get_callers`, `get_blast_radius`, `get_graph_json`) —
    // which the CLI expresses via `--direction deps|callers|both`. Accept those
    // MCP names as aliases so `travsr get_callers <sym>` resolves instead of
    // erroring with "unrecognized subcommand"; add `--direction callers` to
    // narrow it. Both underscore and hyphen spellings work.
    #[command(
        visible_alias = "get_dependencies",
        alias = "get-dependencies",
        alias = "get_callers",
        alias = "get-callers",
        alias = "get_blast_radius",
        alias = "get-blast-radius",
        alias = "get_graph_json",
        alias = "get-graph-json"
    )]
    Graph {
        /// Symbol or file name to start from. Mutually exclusive with --all.
        query: Option<String>,
        /// Path hint to resolve ambiguity: a filename, relative path, directory,
        /// or path fragment (e.g. ppr.rs, crates/travsr-retrieval, retrieval).
        #[arg(long)]
        path: Option<String>,
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
        /// Which git event fired, e.g. `post-commit`, `post-checkout`,
        /// `post-merge`. A commit is described exactly by its own diff; a
        /// checkout or a merge is not, so the two need different file sets.
        #[arg(long)]
        event: Option<String>,
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
    /// Set up and manage full cross-file analysis per language.
    Lang {
        #[command(subcommand)]
        action: lang::LangCommand,
    },
    /// Manage per-repo synonym pairs for richer semantic search.
    Synonym {
        #[command(subcommand)]
        action: synonym::SynonymCommand,
    },
    /// Manage the reranker model that reorders search results by relevance.
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
        /// Log at debug level instead of info.
        ///
        /// The log file is written at info, so debug-only events are absent
        /// from it entirely rather than merely filtered out — `daemon logs
        /// --level debug` cannot recover what was never written. Start with
        /// this when you need them. Costs log volume, so it is not the default.
        #[arg(long, default_value_t = false)]
        verbose: bool,
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
    /// Show the last diagnostics overlay the editor extension reported (#688).
    ///
    /// Answers "is the VS Code overlay reaching the daemon, and what did it
    /// last see". Held in memory by the running daemon, so a restart clears it.
    Lsp,
    /// Print daemon log entries. Reads the file directly, so it works after a
    /// crash and does not need a running daemon.
    Logs {
        /// Stream new lines as they are written, following rotation.
        #[arg(long, short = 'f', default_value_t = false)]
        follow: bool,
        /// Lines to show from the end of the log, spanning rotated files.
        /// 0 prints the whole retained history.
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// Show only lines tagged with this repo, for a log that serves several
        /// of them. Matches the repo tag as written: `--global` logs tag by
        /// name, while this repo's own log tags by full path, so a bare
        /// basename will not match here.
        #[arg(long)]
        repo: Option<String>,
        /// Show only this severity and above: trace, debug, info, warn, error.
        #[arg(long)]
        level: Option<String>,
        /// Show only lines newer than this age, for example 45s, 10m, 2h, 1d.
        #[arg(long)]
        since: Option<String>,
        /// Print the stored JSON lines verbatim instead of rendering them, for
        /// piping into jq or a log collector.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Render times in UTC, as stored, rather than in local time.
        #[arg(long, default_value_t = false)]
        utc: bool,
        /// Read the global log in ~/.travsr instead of this repo's, which is
        /// where `travsr mcp --global` writes.
        #[arg(long, default_value_t = false)]
        global: bool,
    },
}

/// Report any reports this daemon refused because they named a different repo.
///
/// Without this a persistent root mismatch is indistinguishable from every
/// other reason the plane is empty: a closed panel, an uninstalled extension,
/// an expired lease. The client cannot tell either, because it is
/// fire-and-forget and a write that lands on a daemon which then drops the
/// report still looks like a success (#698 review, P3).
///
/// Prints nothing when there is nothing to say, so the common case is
/// unchanged.
fn print_refused_reports(v: &serde_json::Value) {
    let Some(refused) = v.get("refused").and_then(|r| r.as_array()) else {
        return;
    };
    if refused.is_empty() {
        return;
    }
    let served = v
        .get("served_repo_root")
        .and_then(|s| s.as_str())
        .unwrap_or("this repo");
    for entry in refused {
        let root = entry
            .get("repo_root")
            .and_then(|s| s.as_str())
            .unwrap_or("?");
        let count = entry.get("count").and_then(|n| n.as_u64()).unwrap_or(0);
        println!(
            "{count} report{} refused: named {root}, but this daemon serves {served}",
            if count == 1 { " was" } else { "s were" }
        );
    }
    if let Some(extra) = v.get("refused_overflow").and_then(|n| n.as_u64()) {
        if extra > 0 {
            println!("{extra} further refusals named other roots, not listed");
        }
    }
    println!(
        "An editor reports the first workspace folder, so a multi-root window \
         or one opened on a subdirectory names a root no daemon owns."
    );
}

/// Manual runtime construction instead of `#[tokio::main(flavor =
/// "current_thread")]`, solely to bound the blocking pool (#736 item 8).
///
/// The macro leaves `max_blocking_threads` at tokio's default of 512.
/// `spawn_blocking` callers here (store I/O, git subprocess waits, the embed
/// sidecar plumbing) can therefore fan out to 512 OS threads at ~2 MiB of
/// stack each — ~1 GiB of potential thread-stack commitment, which a
/// memory-limited container has no room for and no say in. `4 ×` the
/// container-aware CPU count, clamped to `[8, 64]`, keeps blocking I/O
/// overlapped without letting the pool outgrow the cage: the floor keeps
/// small machines from serialising independent blocking calls, the ceiling
/// bounds the worst case on big hosts.
///
/// Everything else about the entry point is unchanged: same current-thread
/// flavor, same async body (now `async_main`), same panic/exit behavior —
/// `Builder::build()` failure panics just as the macro's expansion does.
fn main() {
    let max_blocking =
        (4 * travsr_plugin_host::resource_limits::effective_cpu_count()).clamp(8, 64);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(max_blocking)
        .build()
        .expect("failed to build the tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
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

    // UX-019: publish the product version (`RELEASE_VERSION`) so the daemon's
    // session-start log reports the same string as `travsr --version` instead of
    // its own workspace crate version (0.7.0). The background daemon is a re-exec
    // of this same binary, so it runs this too.
    //
    // Must stay `RELEASE_VERSION`, not `CARGO_PKG_VERSION`: on a release build
    // those differ, because `RELEASE_VERSION` carries the injected `+<shortsha>`
    // build metadata that identifies which build a tester is running. Reverting
    // this to `CARGO_PKG_VERSION` would silently restore the ambiguity where
    // every build reports a bare `1.0.0`.
    travsr_daemon::set_build_version(RELEASE_VERSION);

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
    // Global stdio MCP serves every registered repo from one process, and its
    // stdout is the protocol channel, so nothing can be printed there. Without
    // a file it logged nowhere durable at all. Same rolling scheme as the
    // daemon, in the global home next to registry.json, so `travsr daemon logs`
    // can read it with the same reader.
    let global_log_dir = match &cli.command {
        Command::Mcp { global: true, .. } => {
            dirs::home_dir().map(|h| travsr_daemon::logfile::log_dir(&h))
        }
        _ => None,
    };
    // Held for the process lifetime: dropping the guard closes the log.
    let _log_guard = if is_daemon {
        None
    } else {
        init_tracing(global_log_dir.as_deref())
    };

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
fn init_tracing(
    file_dir: Option<&std::path::Path>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    // Default to error-only for normal user operation — no internal tracing noise.
    // Set RUST_LOG=info or RUST_LOG=debug to see internals during development.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));

    #[cfg(not(feature = "otlp"))]
    {
        use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
        use tracing_subscriber::util::SubscriberInitExt as _;

        // No file requested: stderr only, unchanged.
        let Some(dir) = file_dir else {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(env_filter)
                .init();
            return None;
        };

        if std::fs::create_dir_all(dir).is_err() {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(env_filter)
                .init();
            return None;
        }
        travsr_daemon::logfile::prune(
            dir,
            travsr_daemon::logfile::LOG_BUDGET_BYTES,
            travsr_daemon::logfile::MAX_LOG_FILES,
        );
        let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .buffered_lines_limit(travsr_daemon::logfile::BUFFERED_LINES)
            .lossy(true)
            .finish(tracing_appender::rolling::daily(
                dir,
                travsr_daemon::logfile::LOG_PREFIX,
            ));

        // The file gets INFO so the log is worth reading, matching the daemon.
        // stderr keeps the caller's filter, which defaults to error: a stdio
        // MCP client should not have its terminal filled with our internals.
        //
        // The file is JSON lines; stderr stays human-readable. One line is one
        // object, so every field is named and typed instead of being recovered
        // by guessing at column positions — `jq`, Loki and Datadog all read it
        // directly, and `travsr daemon logs` renders it back for people rather
        // than making them read JSON. `with_current_span` is on because the
        // repo tag that `--repo` filters by lives in a span, not in the event.
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_filter(tracing_subscriber::EnvFilter::new("info")),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_filter(env_filter),
            )
            .init();
        Some(guard)
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
                    "OTLP exporter init failed, falling back to stderr-only tracing"
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
            force,
            allow_unsandboxed_lsif,
            no_connect,
        } => init::run(
            quiet,
            json,
            jobs,
            semantic,
            force,
            allow_unsandboxed_lsif,
            no_connect,
        )?,
        Command::Connect {
            tool,
            print,
            remove,
            commit,
        } => {
            let cwd = std::env::current_dir()?;
            // Write command: `connect` creates files in the resolved root, so it
            // must stay in the worktree we are standing in. The read resolver
            // redirects a linked worktree to the main worktree (issue #302),
            // which would drop this checkout's AI config into a different one.
            // `travsr init` already wires connect through the write resolver.
            let repo_root = repo::find_git_root_for_write(&cwd)?;
            connect::run(
                &repo_root,
                &connect::ConnectOpts {
                    only: tool,
                    dry_run: print,
                    remove,
                    commit,
                    report: connect::Report::Stdout,
                },
            )?;
        }
        Command::Daemon { action } => {
            let cwd = std::env::current_dir()?;
            // The daemon indexes/watches the resolved root: bind it to the
            // worktree we are in, never the main worktree (issue #586).
            let repo_root = repo::find_git_root_for_write(&cwd)?;
            match action {
                DaemonAction::Start {
                    foreground,
                    verbose,
                } => {
                    if foreground {
                        // The daemon builds its own filter from the environment
                        // inside `run`, so setting it here reaches it. Only when
                        // the caller did not already say what they wanted.
                        if verbose && std::env::var_os("RUST_LOG").is_none() {
                            std::env::set_var("RUST_LOG", "debug");
                        }
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

                        match daemon_client::spawn_background_daemon(&repo_root, &exe, verbose) {
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
                                        "travsr daemon failed to start; see `travsr daemon logs`"
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
                    match send_shutdown_waiting_for_startup(&repo_root) {
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
                            if transport_absent(&e) {
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
                DaemonAction::Lsp => {
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::LspStatus) {
                        // A daemon that predates this variant cannot parse the
                        // message and answers `ok: false` with no `result`
                        // (`daemon_client.rs` documents that response shape).
                        // That reached the arm below as `sessions: None` and
                        // was reported as "no editor attached", which is a
                        // claim about the editor when the fact is about the
                        // daemon's version, and sent people hunting through
                        // extension settings for a problem that did not exist
                        // (#698 review, P3).
                        Ok(resp) if !resp.ok => {
                            println!("daemon: running, but it does not support the editor plane");
                            println!(
                                "It was started before this feature existed. Restart it with \
                                 `travsr daemon restart`."
                            );
                        }
                        Ok(resp) => {
                            let v = resp.result.unwrap_or(serde_json::Value::Null);
                            let sessions = v.get("sessions").and_then(|s| s.as_array()).cloned();
                            match sessions.as_deref() {
                                None | Some([]) => {
                                    println!("no editor attached");
                                    println!(
                                        "The VS Code extension attaches when a graph panel \
                                         renders, and its view expires once the window closes."
                                    );
                                    print_refused_reports(&v);
                                }
                                Some(list) => {
                                    print_refused_reports(&v);
                                    println!(
                                        "{} editor{} attached",
                                        list.len(),
                                        if list.len() == 1 { "" } else { "s" }
                                    );
                                    for s in list {
                                        let n = |k: &str| {
                                            s.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
                                        };
                                        let id = s
                                            .get("session")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("?");
                                        let broken = s
                                            .get("broken")
                                            .and_then(|b| b.as_array())
                                            .cloned()
                                            .unwrap_or_default();
                                        println!();
                                        let plural = |c: u64, word: &str| {
                                            format!("{c} {word}{}", if c == 1 { "" } else { "s" })
                                        };
                                        println!(
                                            "  {id}  ({} seen, updated {}s ago, expires in {}s)",
                                            plural(n("seen"), "file"),
                                            n("age_secs"),
                                            n("expires_in_secs")
                                        );
                                        if broken.is_empty() {
                                            println!("    nothing currently broken");
                                        }
                                        for f in &broken {
                                            let g = |k: &str| {
                                                f.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
                                            };
                                            let path = f
                                                .get("path")
                                                .and_then(|x| x.as_str())
                                                .unwrap_or("");
                                            let mut parts = Vec::new();
                                            if g("errors") > 0 {
                                                parts.push(plural(g("errors"), "error"));
                                            }
                                            if g("warnings") > 0 {
                                                parts.push(plural(g("warnings"), "warning"));
                                            }
                                            println!("    {path}  {}", parts.join(", "));
                                        }
                                        // The difference between "clean" and
                                        // "nothing looked at it", which no count
                                        // of errors can express.
                                        if n("undiagnosed") > 0 {
                                            println!(
                                                "    {} of those files had no diagnostic provider \
                                                 reporting, so they are unknown rather than clean",
                                                n("undiagnosed")
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            println!("daemon: not running, so no editor can be attached");
                            println!("Start it with `travsr daemon start`.");
                        }
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
                        // #685 review: a busy pipe or a timed-out exchange means
                        // something IS listening on the control transport, so the
                        // daemon is up, not starting and not absent. Only
                        // transport-absent errors fall through to the repo-lock
                        // starting/not-running classification below.
                        Err(e) if transport_busy(&e) => {
                            println!("daemon: running (control pipe busy, retry shortly)");
                        }
                        Err(e) if !transport_absent(&e) => {
                            println!("daemon: running (not responding: {e:#})");
                        }
                        Err(_) => {
                            // Socket not ready yet — the daemon may be alive but
                            // still starting (store open, embed sidecar model
                            // load, initial watcher scan; tens of seconds). The
                            // liveness authority for that window is the repo-lock
                            // flock, NOT the lock-file PID: on Windows the
                            // daemon's exclusive flock makes the PID read fail
                            // with a lock violation for its entire lifetime, so
                            // the old PID fallback reported "not running" against
                            // a live, still-starting daemon.
                            if daemon_client::daemon_lock_held(&repo_root) {
                                println!("daemon: starting (control socket not ready yet)");
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
                    // Deliver the stop through the daemon's startup window,
                    // exactly as `daemon stop` does. The old fire-and-forget
                    // Shutdown was silently lost while the daemon was between
                    // taking daemon.lock and binding its control transport;
                    // the lock wait below then expired, the spawn reported
                    // AlreadyRunning, and the generic success arm still
                    // printed "restarted" with exit 0 while the old daemon
                    // kept running and no new daemon existed.
                    match send_shutdown_waiting_for_startup(&repo_root) {
                        Ok(_) => {}
                        // Not running: restart degrades to a plain start.
                        Err(e) if transport_absent(&e) => {}
                        Err(e) => {
                            return Err(e.context(
                                "the running daemon could not be stopped, so it was NOT \
                                 restarted; check `travsr daemon status` and retry",
                            ));
                        }
                    }
                    // Wait for the old daemon to actually release its lock
                    // instead of a fixed sleep, so the new one never races the
                    // old one's shutdown and fails to acquire the lock.
                    // Budgeted like `daemon stop`'s exit wait: a clean
                    // shutdown can spend seconds flushing the store.
                    let lock_cutoff = std::time::Instant::now() + STOP_EXIT_TIMEOUT;
                    while daemon_client::daemon_lock_held(&repo_root)
                        && std::time::Instant::now() < lock_cutoff
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    let exe = std::env::current_exe().context("finding current exe")?;
                    // Relay the new daemon's startup, exactly as `daemon start`
                    // does. #348 is about a command that detaches in under 10ms
                    // and tells you nothing, and restart detaches the same way:
                    // it printed one line and returned the prompt whether the
                    // replacement came up or died. Restart is also the command
                    // you run right after a rebuild, which is precisely when
                    // "did the new one actually start" is the question.
                    //
                    // Seek to the end before spawning so the relay shows this
                    // session's lines rather than replaying the old daemon's.
                    let mut relay = travsr_daemon::logfile::LogTail::new(
                        &travsr_daemon::logfile::log_dir(&repo_root),
                    );
                    relay.seek_to_end();
                    let started = std::time::Instant::now();
                    match daemon_client::spawn_background_daemon(&repo_root, &exe, false) {
                        daemon_client::SpawnOutcome::Failed => match daemon_start_error(&repo_root)
                        {
                            Some(r) => anyhow::bail!("travsr daemon failed to restart: {r}"),
                            None => anyhow::bail!(
                                "travsr daemon failed to restart (see `travsr daemon logs`)"
                            ),
                        },
                        // The stop was accepted, but the old process still
                        // holds the repo lock past the wait above, so nothing
                        // was spawned. Surface that instead of claiming
                        // success (the exact false positive `daemon stop`
                        // refuses to report).
                        daemon_client::SpawnOutcome::AlreadyRunning => anyhow::bail!(
                            "travsr daemon was NOT restarted: the old daemon still holds \
                             the repo lock {}s after accepting the stop.\n  \
                             Check `travsr daemon status`, then retry `travsr daemon restart`.",
                            STOP_EXIT_TIMEOUT.as_secs()
                        ),
                        _ => relay_daemon_startup(&repo_root, relay, started),
                    }
                }
                DaemonAction::Logs {
                    follow,
                    lines,
                    repo,
                    level,
                    since,
                    json,
                    utc,
                    global,
                } => {
                    let dir = if global {
                        travsr_daemon::logfile::log_dir(
                            &dirs::home_dir()
                                .context("cannot determine home directory for --global")?,
                        )
                    } else {
                        travsr_daemon::logfile::log_dir(&repo_root)
                    };
                    // Non-global logs belong to exactly one repo, so the
                    // renderer can leave its name and paths implicit.
                    let renderer_repo = if global {
                        None
                    } else {
                        Some(repo_root.clone())
                    };
                    let min_level = level.as_deref().map(parse_level).transpose()?;
                    let since = since.as_deref().map(parse_since).transpose()?;
                    // Asking for a level the file cannot contain must not look
                    // like "nothing happened". The file layer writes at info, so
                    // debug and trace lines are absent unless the daemon was
                    // started for them — a silent empty result here reads as a
                    // fact about the system when it is a fact about the filter.
                    let asked_below_info = matches!(
                        level.as_deref().map(str::to_ascii_lowercase).as_deref(),
                        Some("debug") | Some("trace")
                    );
                    if asked_below_info && !follow {
                        eprintln!(
                            "note: the log file is written at info, so debug and trace lines are \
                             only present if the daemon was started with `travsr daemon start \
                             --verbose` (or RUST_LOG). Restart it that way to capture them."
                        );
                    }
                    daemon_logs(
                        &dir,
                        follow,
                        lines,
                        LineFilter::new(repo, min_level, since),
                        json,
                        utc,
                        renderer_repo,
                    )?;
                }
                DaemonAction::StopEmbed => {
                    match send_daemon_command(&repo_root, &travsr_ipc::ControlMessage::StopEmbed) {
                        Ok(resp) => eprintln!(
                            "{}",
                            resp.message
                                .unwrap_or_else(|| "embed auto-reindex paused".into())
                        ),
                        Err(e) => {
                            if transport_absent(&e) {
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
                            if transport_absent(&e) {
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
                            "data": "travsr: not initialized; run `travsr init` first, then retry"
                        }
                    });
                    println!("{err_msg}");
                    anyhow::bail!("not initialized; run `travsr init` first");
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
            path,
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
            (false, Some(q)) => graph::run(
                q,
                path,
                depth,
                direction,
                format,
                edges,
                include_noise,
                budget,
            )?,
            (false, None) => anyhow::bail!("provide a symbol/file query or pass --all"),
        },
        Command::HookRun {
            from_hook,
            event,
            paths,
        } => {
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
            // A branch checkout or a fast-forward merge changes the tree
            // without producing a commit that describes the change, so
            // `git diff-tree HEAD` reports the new tip's own diff and says
            // nothing about the files that differ between the two trees.
            // Reindexing that delta leaves every other changed file describing
            // a tree that is no longer checked out, and leaves files the
            // checkout deleted in the graph as ghosts: `travsr ask` then
            // answers with paths that are not on disk.
            //
            // The daemon does not need this. Its watcher sees the same
            // deletions and reconciles them, which is why the gap only shows up
            // without one. Verified both ways before choosing where to fix it.
            let whole_tree = matches!(event.as_deref(), Some("post-checkout") | Some("post-merge"));
            let dirty = if from_hook && whole_tree {
                let (dirty, files) = travsr_daemon::reconcile_tracked_tree(&repo_root, &mut store)?;
                tracing::debug!(
                    event = event.as_deref().unwrap_or(""),
                    files,
                    "hook-run: reconciled the whole tracked tree"
                );
                // Phase A now describes the checked-out tree, so say so. Left
                // unstamped, `travsr status` would print an index/HEAD mismatch
                // note after every branch switch even though the graph is
                // correct, and a warning that fires when nothing is wrong is
                // one users learn to skip past.
                //
                // Guarded the way `reconcile_head_drift` guards it: never claim
                // freshness for a reindex that `reindex_files` skipped wholesale
                // because the stored signature format is from an older travsr.
                // Phase B is a separate marker and stays stale, which is honest:
                // this path rebuilds Phase A only, and `travsr status` reports
                // the two separately.
                if store.get_signature_format_version().ok()
                    == Some(travsr_core::SIGNATURE_FORMAT_VERSION)
                {
                    if let Ok(head) = travsr_daemon::read_head_commit_sha(&repo_root) {
                        if !head.is_empty() {
                            let _ = store.set_meta("last_commit", &head);
                        }
                    }
                }
                dirty
            } else {
                let abs_paths: Vec<std::path::PathBuf> = if from_hook {
                    travsr_daemon::changed_files_from_git(&repo_root)?
                } else {
                    paths.iter().map(|p| repo_root.join(p)).collect()
                };
                travsr_daemon::reindex_files(&abs_paths, &repo_root, &mut store)?
            };
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
    // The log on disk is JSON; a person waiting at a prompt is not going to read
    // it. Render the same columns `travsr daemon logs` uses, minus the date
    // separator, which is today by construction here.
    //
    // Scoped to the repo for the same reason `daemon logs` is: the reader just
    // typed the command in this directory, so `repo=/long/path/to/it` on every
    // line is thirty characters spent restating that.
    let render = LogRenderer::new(false).for_repo(repo_root.to_path_buf());
    eprintln!("[travsr] starting…");

    let drain = |relay: &mut travsr_daemon::logfile::LogTail| {
        if let Ok(lines) = relay.poll() {
            for line in lines {
                eprintln!("[travsr] {}", render.one_line(&LogLine::parse(&line)));
            }
        }
    };

    loop {
        drain(&mut relay);

        // One attempt, no internal sleep: this loop already paces itself, and a
        // nested delay would double the interval and blur the elapsed figure.
        if daemon_is_running(repo_root, 1, 0) {
            // Readiness is the socket answering, not the log catching up. The
            // startup lines are flushed on the writer's own cadence, so the
            // socket can go live between the poll above and this check, leaving
            // the last few lines on disk but unrelayed. One final drain empties
            // what landed in that window before the "ready" line closes the
            // stream. Lines flushed strictly after this belong to the running
            // daemon, not to startup, so `--follow` is the right place for them.
            drain(&mut relay);
            eprintln!("[travsr] ready in {:.1}s", started.elapsed().as_secs_f32());
            return;
        }
        if std::time::Instant::now() >= deadline {
            // Not an error: the daemon may simply still be scanning. Say what
            // is known rather than claiming a failure that may not have
            // happened.
            eprintln!(
                "[travsr] still starting after {}s, follow it with `travsr daemon logs --follow`",
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
    // Quoted rendering (string-valued fields): the closing quote is its own
    // terminator, so a plain match is exact.
    if line.contains(&format!("repo=\"{repo}\"")) {
        return true;
    }
    // Unquoted rendering (Display-valued fields, e.g. the daemon's own
    // `repo=/path/to/repo`): the value runs to the next separator, so the match
    // has to end at one. Without this, `--repo alpha` silently also selects
    // every line belonging to `alpha-staging`.
    let needle = format!("repo={repo}");
    line.match_indices(&needle).any(|(i, _)| {
        line[i + needle.len()..]
            .chars()
            .next()
            // `is_none_or` is stable since 1.82; MSRV here is 1.75.
            .map_or(true, |c| c.is_whitespace() || c == '}' || c == ',')
    })
}

/// Severity rank, ordered so `>=` reads as "at least this severe".
fn level_rank(name: &str) -> Option<u8> {
    match name {
        "TRACE" => Some(0),
        "DEBUG" => Some(1),
        "INFO" => Some(2),
        "WARN" => Some(3),
        "ERROR" => Some(4),
        _ => None,
    }
}

/// Parse a `--level` argument.
fn parse_level(s: &str) -> anyhow::Result<u8> {
    level_rank(&s.to_ascii_uppercase()).with_context(|| {
        format!("unknown log level `{s}` (expected trace, debug, info, warn or error)")
    })
}

/// Parse a `--since` argument: `45s`, `10m`, `2h`, `1d`.
///
/// A duration rather than a clock time on purpose. Log timestamps are UTC and
/// the reader's clock is not, so "since 14:20" invites a five-and-a-half hour
/// mistake that "since 10m" cannot make.
fn parse_since(s: &str) -> anyhow::Result<chrono::Duration> {
    let unit = s
        .chars()
        .last()
        .with_context(|| "`--since` needs a value, for example `10m`")?;
    let digits = &s[..s.len() - unit.len_utf8()];
    let n: i64 = digits.parse().map_err(|_| {
        anyhow::anyhow!("`--since {s}`: expected a number followed by s, m, h or d")
    })?;
    if n < 0 {
        anyhow::bail!("`--since {s}`: a duration cannot be negative");
    }
    match unit.to_ascii_lowercase() {
        's' => Ok(chrono::Duration::seconds(n)),
        'm' => Ok(chrono::Duration::minutes(n)),
        'h' => Ok(chrono::Duration::hours(n)),
        'd' => Ok(chrono::Duration::days(n)),
        _ => anyhow::bail!("`--since {s}`: unknown unit `{unit}` (expected s, m, h or d)"),
    }
}

/// Whether a line starts a log entry, as opposed to continuing one.
///
/// Only plain-text lines can continue: a JSON entry is one object per line with
/// any newlines escaped inside it. Text entries begin with an RFC3339 timestamp;
/// a panic backtrace frame does not, and carries neither a level nor a timestamp
/// of its own, so every filter has to recognise it rather than judge it.
fn is_entry_start(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T'
}

/// One line of the log.
///
/// The daemon writes JSON lines. Rotated files written before that change are
/// still on disk and still worth reading, and a torn write is not JSON either,
/// so anything that does not parse is carried through as opaque text rather
/// than dropped. This is the only reason the text path still exists.
enum LogLine {
    Json(serde_json::Value),
    Text(String),
}

impl LogLine {
    fn parse(line: &str) -> Self {
        // Cheap reject before handing bytes to the parser: every entry is an
        // object, so a line that does not open with `{` cannot be one.
        if line.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("timestamp").is_some() && v.get("level").is_some() {
                    return Self::Json(v);
                }
            }
        }
        Self::Text(line.to_string())
    }

    /// RFC3339 timestamp, if the line has one.
    fn timestamp(&self) -> Option<&str> {
        match self {
            Self::Json(v) => v.get("timestamp").and_then(|t| t.as_str()),
            // Text entries start with the timestamp; `%Y-%m-%dT%H:%M:%S` is the
            // first 19 bytes and the fraction runs to the `Z`.
            Self::Text(s) => s.split_whitespace().next().filter(|t| is_entry_start(t)),
        }
    }

    fn level(&self) -> Option<u8> {
        match self {
            Self::Json(v) => v.get("level").and_then(|l| l.as_str()).and_then(level_rank),
            Self::Text(s) => s.split_whitespace().nth(1).and_then(level_rank),
        }
    }

    /// This line as a JSON object, whatever it started as.
    ///
    /// `--json` exists to be piped into `jq` or a log collector, so the stream
    /// has to be uniformly valid: one line that is not an object kills the
    /// consumer on the spot, and it does not matter that the other 16 lines were
    /// fine. Rotated files written before the format changed are most of the
    /// history for a week after it, so this is the normal case rather than an
    /// exotic one.
    ///
    /// Such lines are wrapped rather than dropped, tagged `unparsed` so a
    /// consumer can tell them apart, and given whatever timestamp, level and
    /// target can be recovered from the old rendering so they stay queryable
    /// instead of opaque.
    fn to_json_line(&self) -> String {
        match self {
            Self::Json(v) => v.to_string(),
            Self::Text(s) => {
                let mut obj = serde_json::Map::new();
                let mut message: &str = s;
                if is_entry_start(s) {
                    let mut tok = s.split_whitespace();
                    if let Some(ts) = tok.next() {
                        obj.insert("timestamp".into(), ts.into());
                    }
                    if let Some(level) = tok.next().filter(|l| level_rank(l).is_some()) {
                        obj.insert("level".into(), level.into());
                    }
                    if let Some(target) = tok.next() {
                        obj.insert("target".into(), target.trim_end_matches(':').into());
                    }
                    message = legacy_message(s);
                }
                let mut fields = serde_json::Map::new();
                fields.insert("message".into(), message.into());
                obj.insert("fields".into(), serde_json::Value::Object(fields));
                obj.insert("unparsed".into(), true.into());
                serde_json::Value::Object(obj).to_string()
            }
        }
    }

    /// Whether this line belongs to `repo`. JSON puts the tag in a named field,
    /// on the event or on the span it happened inside, so no string matching is
    /// needed; text lines fall back to matching the rendering.
    fn is_for_repo(&self, repo: &str) -> bool {
        match self {
            Self::Json(v) => ["fields", "span"].iter().any(|k| {
                v.get(k)
                    .and_then(|o| o.get("repo"))
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| r == repo)
            }),
            Self::Text(s) => line_is_for_repo(s, repo),
        }
    }
}

/// The message of a pre-JSON log line: everything after the fixed
/// `<timestamp> <LEVEL> <target>:` prefix.
///
/// Scans past exactly three whitespace-separated tokens rather than splitting on
/// `": "`, because messages contain that too (`#478: backfilling ...`) and would
/// otherwise be cut in half.
fn legacy_message(line: &str) -> &str {
    let b = line.as_bytes();
    let mut i = 0;
    for _ in 0..3 {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        while i < b.len() && b[i] != b' ' {
            i += 1;
        }
    }
    line[i..].trim_start()
}

/// Shorten a tracing target to the subsystem a reader cares about.
///
/// `travsr_plugin_host::registry` is 29 characters that say "plugin host". The
/// module tail rarely disambiguates anything a message does not already say.
fn short_target(target: &str) -> String {
    target
        .split("::")
        .next()
        .unwrap_or(target)
        .trim_start_matches("travsr_")
        .replace('_', "-")
}

/// Render a field value without JSON's quoting noise, quoting only when the
/// value would otherwise look like two fields.
fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => {
            if s.contains(char::is_whitespace) {
                format!("\"{s}\"")
            } else {
                s.clone()
            }
        }
        other => other.to_string(),
    }
}

/// Turns log lines into columns, and announces the date when it changes.
///
/// The date is a separator rather than a prefix on every line: it changes once
/// a day and costs 27 characters per entry to repeat, which is most of the room
/// before the message on an 80-column terminal. Times are local, because the
/// file is UTC and the reader's clock generally is not; the separator names the
/// zone so the two are never confused.
struct LogRenderer {
    last_date: Option<String>,
    utc: bool,
    /// Repo the log belongs to, when it serves exactly one.
    ///
    /// Used to drop what the reader already knows. Every repo-scoped line
    /// carries `repo=/Users/me/work/travsr`, which is thirty-odd characters of a
    /// fact established by the directory the command was run in, repeated on
    /// every line. `--global` leaves this unset, because there the repo is the
    /// one thing that distinguishes one line from the next.
    repo: Option<PathBuf>,
}

impl LogRenderer {
    fn new(utc: bool) -> Self {
        Self {
            last_date: None,
            utc,
            repo: None,
        }
    }

    /// Treat this log as belonging to a single repo, whose name and paths can
    /// therefore be left implicit.
    fn for_repo(mut self, repo: PathBuf) -> Self {
        self.repo = Some(repo);
        self
    }

    /// Local (or UTC) date and time-of-day for an RFC3339 timestamp.
    fn split_stamp(&self, ts: &str) -> Option<(String, String)> {
        let parsed = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
        if self.utc {
            let t = parsed.with_timezone(&chrono::Utc);
            Some((
                t.format("%Y-%m-%d UTC").to_string(),
                t.format("%H:%M:%S").to_string(),
            ))
        } else {
            let t = parsed.with_timezone(&chrono::Local);
            Some((
                t.format("%Y-%m-%d %Z").to_string(),
                t.format("%H:%M:%S").to_string(),
            ))
        }
    }

    /// The lines to print for one log line: an optional date separator, then the
    /// entry itself.
    fn render(&mut self, line: &LogLine) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((date, _)) = line.timestamp().and_then(|ts| self.split_stamp(ts)) {
            if self.last_date.as_deref() != Some(date.as_str()) {
                out.push(format!("── {date} ──"));
                self.last_date = Some(date);
            }
        }
        out.push(self.one_line(line));
        out
    }

    /// One entry, without the date separator. The startup relay uses this: the
    /// date is today by construction and the user is watching it happen.
    fn one_line(&self, line: &LogLine) -> String {
        if let LogLine::Json(v) = line {
            if let Some((_, time)) = line.timestamp().and_then(|ts| self.split_stamp(ts)) {
                return self.columns(v, &time);
            }
        }
        // Text: already human-readable, and reformatting it would mean parsing a
        // format that no longer exists. Pass it through.
        match line {
            LogLine::Text(s) => s.clone(),
            LogLine::Json(v) => v.to_string(),
        }
    }

    fn columns(&self, v: &serde_json::Value, time: &str) -> String {
        // INFO is left blank. It is the level of nine lines in ten, so printing
        // it is four characters of "nothing unusual happened" per line, and it
        // buries the two lines that do say something. WARN and ERROR keep their
        // label and now stand out from a column of blanks. `--json` still
        // carries the level on every entry for anything that filters on it.
        let level = match v.get("level").and_then(|l| l.as_str()) {
            Some("INFO") | None => "",
            Some(other) => other,
        };
        let target = v
            .get("target")
            .and_then(|t| t.as_str())
            .map(short_target)
            .unwrap_or_default();
        let fields = v.get("fields").and_then(|f| f.as_object());
        let message = fields
            .and_then(|f| f.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let rest: Vec<String> = fields
            .map(|f| {
                f.iter()
                    .filter(|(k, _)| k.as_str() != "message")
                    .filter(|(k, val)| !self.is_redundant(k, val))
                    .map(|(k, val)| format!("{k}={}", self.shorten(val)))
                    .collect()
            })
            .unwrap_or_default();

        let mut line = format!("{time}  {level:<5}  {target:<11}  {message}");
        if !rest.is_empty() {
            line.push(' ');
            line.push_str(&rest.join(" "));
        }
        line
    }

    /// Whether a field says something the reader established by running the
    /// command where they ran it.
    fn is_redundant(&self, key: &str, value: &serde_json::Value) -> bool {
        let Some(repo) = &self.repo else {
            return false;
        };
        key == "repo" && value.as_str().is_some_and(|v| Path::new(v) == repo)
    }

    /// Render a value, with paths inside the repo shortened to what varies.
    ///
    /// `sock=/Users/me/work/travsr/.travsr/daemon-1c0d66a3.sock` is seventy
    /// characters to say which socket, of which only the last component differs
    /// between two of them.
    fn shorten(&self, value: &serde_json::Value) -> String {
        if let (Some(repo), Some(s)) = (&self.repo, value.as_str()) {
            if let Ok(rel) = Path::new(s).strip_prefix(repo) {
                let shown = rel.to_string_lossy();
                if !shown.is_empty() {
                    return shown.into_owned();
                }
            }
        }
        render_value(value)
    }
}

/// Which log lines `travsr daemon logs` prints.
///
/// Stateful because of continuation lines: judging a backtrace frame on its own
/// merits would strip it from the entry it belongs to, leaving an error message
/// whose cause was filtered out from underneath it. Each continuation inherits
/// the decision made for the entry above it.
struct LineFilter {
    repo: Option<String>,
    min_level: Option<u8>,
    /// Cutoff as an RFC3339 prefix. Log timestamps are fixed-width UTC, so a
    /// string comparison is already a chronological one — no parsing per line.
    since: Option<String>,
    last_kept: bool,
}

impl LineFilter {
    fn new(repo: Option<String>, min_level: Option<u8>, since: Option<chrono::Duration>) -> Self {
        Self {
            repo,
            min_level,
            since: since.map(|d| {
                (chrono::Utc::now() - d)
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string()
            }),
            last_kept: true,
        }
    }

    /// Whether every active filter admits this line.
    fn keep(&mut self, line: &LogLine) -> bool {
        // Only a text line can be a continuation, and only then does inheritance
        // apply. Every JSON line is a complete entry.
        if let LogLine::Text(s) = line {
            if !is_entry_start(s) {
                return self.last_kept;
            }
        }
        let kept = self.admits(line);
        self.last_kept = kept;
        kept
    }

    fn admits(&self, line: &LogLine) -> bool {
        if let Some(repo) = &self.repo {
            if !line.is_for_repo(repo) {
                return false;
            }
        }
        if let Some(min) = self.min_level {
            // A line whose level is unreadable is shown rather than hidden:
            // dropping it would let a format change silently empty the output.
            match line.level() {
                Some(rank) if rank < min => return false,
                _ => {}
            }
        }
        if let Some(cutoff) = &self.since {
            // Timestamps are fixed-width RFC3339 UTC, so comparing the prefix as
            // a string is already comparing instants. A line with no timestamp
            // has no age to judge and is kept.
            if let Some(ts) = line.timestamp() {
                if ts.len() < cutoff.len() || &ts[..cutoff.len()] < cutoff.as_str() {
                    return false;
                }
            }
        }
        true
    }
}

/// `travsr daemon logs` — print, and optionally follow, the daemon log.
///
/// Reads the file rather than asking the daemon, so it still works after a
/// crash, which is when it is most wanted. Output carries no ANSI: these lines
/// get piped into `grep` far more often than they get read directly.
fn daemon_logs(
    dir: &std::path::Path,
    follow: bool,
    lines: usize,
    mut filter: LineFilter,
    raw: bool,
    utc: bool,
    repo: Option<PathBuf>,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut render = match repo {
        Some(r) => LogRenderer::new(utc).for_repo(r),
        None => LogRenderer::new(utc),
    };
    let mut tail = travsr_daemon::logfile::LogTail::new(dir);

    if tail.path().is_none() {
        // Name the directory, not a filename: the log is dated, so telling the
        // user to look for `daemon.log` would send them after a file that is
        // never created.
        eprintln!(
            "no daemon log in {} yet; run `travsr daemon start`",
            dir.display()
        );
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let backfill = tail.backfill(lines)?;
    let mut shown = 0usize;
    let mut scanned = 0usize;
    for text in backfill.lines() {
        scanned += 1;
        let line = LogLine::parse(text);
        if !filter.keep(&line) {
            continue;
        }
        shown += 1;
        if raw {
            writeln!(out, "{}", line.to_json_line())?;
        } else {
            for rendered in render.render(&line) {
                writeln!(out, "{rendered}")?;
            }
        }
    }
    out.flush()?;

    // A filter that matched nothing looks exactly like a daemon that logged
    // nothing. Say which it was, on stderr so it never reaches a pipe.
    if shown == 0 && scanned > 0 {
        eprintln!("no matching lines in the last {scanned} log line(s), widen the filters");
    }

    if !follow {
        return Ok(());
    }

    // Only lines from here on; the backfill above already covered history.
    tail.seek_to_end();
    loop {
        for text in tail.poll()? {
            let line = LogLine::parse(&text);
            if !filter.keep(&line) {
                continue;
            }
            if raw {
                writeln!(out, "{}", line.to_json_line())?;
            } else {
                for rendered in render.render(&line) {
                    writeln!(out, "{rendered}")?;
                }
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

/// How long `daemon stop` waits for a still-starting daemon (repo lock held,
/// control transport not bound yet) to become reachable before giving up on
/// delivering the stop. The pre-bind stretch covers the store open, the embed
/// sidecar loading its model, and the initial watcher scan — tens of seconds
/// on big repos or with large embedding models.
const STOP_DELIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// True when a control-transport error means nothing is listening on the
/// repo's socket/pipe: the endpoint does not exist ("No such file" /
/// "os error 2"), or a stale Unix socket has no daemon accepting behind it
/// ("Connection refused" — os error 111 on Linux, 61 on macOS).
fn transport_absent(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("No such file")
        || msg.contains("Connection refused")
        || msg.contains("os error 2")
        || msg.contains("os error 111")
        || msg.contains("os error 61")
}

/// True when the error means a daemon IS listening but every pipe instance was
/// serving another client when we tried to connect (Windows `ERROR_PIPE_BUSY`,
/// surfaced by `NamedPipeTransport::connect` as "daemon is busy (...)").
///
/// #685 review: this is the one transport error that is both "the daemon is
/// definitely up" and "transient by construction" (the daemon re-creates a
/// pipe instance on every accept-loop iteration), so `status` must not report
/// it as still-starting and `stop` should retry it rather than give up.
fn transport_busy(e: &anyhow::Error) -> bool {
    e.to_string().contains("daemon is busy")
}

/// Send `Shutdown`, waiting out the daemon's startup window if needed.
///
/// Between taking `daemon.lock` and binding its control transport the daemon
/// spends its longest startup stretch (store open, embed sidecar model load,
/// initial watcher scan), and a connect in that window fails exactly like "no
/// daemon at all". Treating it that way silently loses the stop: the CLI
/// reports "not running", the daemon finishes starting, and it keeps running
/// (and serving) a repo its owner believes is stopped. While the repo lock is
/// held by a live daemon, keep retrying until the transport binds, the daemon
/// exits on its own (lock released — the caller then sees the usual "not
/// running" transport error), or [`STOP_DELIVER_TIMEOUT`] fires.
///
/// A busy control pipe (Windows: every instance serving another client) is
/// retried under the same deadline (#685 review): the daemon is provably up
/// and the contention clears as soon as its accept loop creates the next
/// instance, so it sits in the same "worth another attempt" bucket as the
/// startup window.
fn send_shutdown_waiting_for_startup(
    repo_root: &std::path::Path,
) -> anyhow::Result<travsr_ipc::ControlResponse> {
    let cutoff = std::time::Instant::now() + STOP_DELIVER_TIMEOUT;
    let mut announced = false;
    loop {
        let err = match send_daemon_command(repo_root, &travsr_ipc::ControlMessage::Shutdown) {
            Ok(resp) => return Ok(resp),
            Err(e) => e,
        };
        // #685 review: a busy pipe (Windows, all instances mid-request) is as
        // transient as the startup window this loop already absorbs, and the
        // daemon is provably up. Retry it too, instead of failing `daemon
        // stop` on the first contended connect.
        let busy = transport_busy(&err);
        let starting = transport_absent(&err) && daemon_client::daemon_lock_held(repo_root);
        if !busy && !starting {
            return Err(err);
        }
        if std::time::Instant::now() >= cutoff {
            if busy {
                return Err(err.context(format!(
                    "the daemon's control pipe stayed busy for {}s and the stop \
                     request was NOT delivered; retry `travsr daemon stop`",
                    STOP_DELIVER_TIMEOUT.as_secs()
                )));
            }
            anyhow::bail!(
                "travsr daemon is still starting (repo lock held, control transport not \
                 bound within {}s) and the stop request was NOT delivered.\n  \
                 Wait for `travsr daemon status` to report it running, then retry \
                 `travsr daemon stop`.",
                STOP_DELIVER_TIMEOUT.as_secs()
            );
        }
        if starting && !announced {
            eprintln!(
                "travsr daemon is starting (control transport not bound yet); \
                 waiting to deliver the stop..."
            );
            announced = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
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
/// Also checks the repo-lock flock so that a daemon that is alive but still
/// starting (socket not bound yet) is not mistaken for "not running" — which
/// would cause a second daemon to be spawned and crash immediately with
/// "another daemon already running".
pub(crate) fn daemon_is_running(repo_root: &std::path::Path, attempts: u32, delay_ms: u64) -> bool {
    for i in 0..attempts {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }
        if send_daemon_command(repo_root, &travsr_ipc::ControlMessage::Status).is_ok() {
            return true;
        }
    }
    // Socket not ready — fall back to the repo-lock flock, the race-free
    // liveness authority. Not the lock-file PID: on Windows the daemon's
    // exclusive flock makes that read fail with a lock violation for as long
    // as the daemon lives, which reported a live-but-still-starting daemon
    // as absent (the exact case this fallback exists for).
    daemon_client::daemon_lock_held(repo_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC-025 §5.5 honesty test (b): a declared floor may never sit above what
    /// users can actually install. For every catalog spec with a real floor
    /// (`min_version > 0.0.0`), the latest released tag must be `>= min_version`.
    ///
    /// Network test, skippable offline: any fetch failure (no network, rate
    /// limit) skips that spec rather than failing, so CI without egress is green.
    #[test]
    fn declared_floor_never_exceeds_latest_release() {
        use travsr_plugin_host::sidecar_version::{Semver, SidecarSpec};

        let mut checked = 0usize;
        let mut check = |spec: &dyn SidecarSpec| {
            let floor = spec.min_version();
            if floor == Semver::ZERO {
                return; // no active floor -> trivially satisfied
            }
            let repo = spec.github_repo().to_string();
            let Ok(tag) = crate::lang::run_async(async move {
                crate::install::fetch_latest_version_for_repo(&repo).await
            }) else {
                eprintln!(
                    "skipping floor<=latest for {} (fetch failed - offline?)",
                    spec.install_name()
                );
                return;
            };
            let Some(latest) = Semver::parse(&tag) else {
                eprintln!(
                    "skipping {}: latest tag '{tag}' unparseable",
                    spec.install_name()
                );
                return;
            };
            assert!(
                floor <= latest,
                "{}: declared floor {floor} is above the latest release {latest} - users cannot satisfy it",
                spec.install_name(),
            );
            checked += 1;
        };

        for b in travsr_plugin_host::embed_backends() {
            check(b);
        }
        // Phase B specs all declare Semver::ZERO today, so they are skipped by
        // the guard above; the loop keeps the test correct if a floor is added.
        for e in travsr_plugin_host::phase_b::catalog::CATALOG {
            use travsr_plugin_host::phase_b::catalog::ScipInstall;
            match &e.scip_install {
                ScipInstall::GithubBinary(s) => check(s),
                ScipInstall::ZipBinary(z) => check(z),
                _ => {}
            }
        }

        // Not an assertion on `checked` > 0: fully offline CI legitimately checks
        // nothing. The value is that when the network IS present, a floor set
        // above the latest release fails loudly.
        let _ = checked;
    }

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

    /// `daemon stop`/`status` must treat only "nothing is listening" errors as
    /// "not running"; anything else (busy pipe, wedged daemon, timeout) has to
    /// surface, or a live daemon gets reported as absent.
    #[test]
    fn transport_absent_classifies_no_listener_errors() {
        for msg in [
            // Windows: pipe path does not exist.
            r"daemon not running (\\.\pipe\travsr-abc): The system cannot \
              find the file specified. (os error 2)",
            // Unix: socket file missing / stale socket with no acceptor.
            "No such file or directory (os error 2)",
            "Connection refused (os error 111)",
            "Connection refused (os error 61)",
        ] {
            assert!(transport_absent(&anyhow::anyhow!(msg)), "{msg}");
        }
        for msg in [
            "daemon is busy (\\\\.\\pipe\\travsr-abc had no free pipe instance for 2s)",
            "control response exceeded deadline",
        ] {
            assert!(!transport_absent(&anyhow::anyhow!(msg)), "{msg}");
        }
    }

    /// #685 review: `status` and `stop` special-case a contended-but-alive
    /// daemon, so the busy classifier must match exactly the connect error the
    /// named-pipe transport produces and nothing else.
    #[test]
    fn transport_busy_matches_only_the_busy_pipe_error() {
        assert!(transport_busy(&anyhow::anyhow!(
            "daemon is busy (\\\\.\\pipe\\travsr-abc had no free pipe instance for 2s); \
             retry shortly"
        )));
        for msg in [
            "daemon not running (\\\\.\\pipe\\travsr-abc): os error 2",
            "Connection refused (os error 111)",
            "timed out after 20s talking to the daemon over the named pipe",
        ] {
            assert!(!transport_busy(&anyhow::anyhow!(msg)), "{msg}");
        }
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

#[cfg(test)]
mod daemon_log_tests {
    use super::line_is_for_repo;

    /// The tag is a span field, so the formatter renders it quoted. Matching on
    /// the rendering keeps the filter independent of the rest of the line.
    #[test]
    fn a_tagged_line_matches_its_own_repo_only() {
        let line =
            r#"INFO mcp.tool_call{tool="search_symbol" req=42 repo="alpha" global=true}: served"#;
        assert!(line_is_for_repo(line, "alpha"));
        assert!(!line_is_for_repo(line, "beta"));
    }

    #[test]
    fn an_untagged_line_belongs_to_no_repo() {
        // Process-wide lines — session start, reranker fetch — carry no repo,
        // and must not be attributed to whichever one the user asked about.
        let line = "INFO travsr_mcp::rerank: reranker model absent, auto-fetching";
        assert!(!line_is_for_repo(line, "alpha"));
    }

    #[test]
    fn a_repo_name_that_is_a_prefix_of_another_does_not_match_it() {
        // "alpha" must not select lines belonging to "alpha-staging", which a
        // bare substring search on the name alone would do.
        let line = r#"INFO mcp.tool_call{repo="alpha-staging"}: served"#;
        assert!(line_is_for_repo(line, "alpha-staging"));
        assert!(!line_is_for_repo(line, "alpha"));
    }

    #[test]
    fn the_unquoted_rendering_is_prefix_safe_too() {
        // Display-valued fields render unquoted: the daemon's own session line
        // is `repo=/path/to/repo`. A bare `contains` on that form matches any
        // longer name that starts with the one asked for.
        let line = "INFO daemon starting event=\"daemon.session.start\" repo=alpha-staging";
        assert!(line_is_for_repo(line, "alpha-staging"));
        assert!(
            !line_is_for_repo(line, "alpha"),
            "alpha must not select alpha-staging"
        );
    }

    // ── --level / --since ────────────────────────────────────────────────

    use super::{is_entry_start, parse_level, parse_since, LineFilter, LogLine, LogRenderer};
    use super::{resolve_version, Cli, RELEASE_VERSION};
    use clap::CommandFactory as _;

    /// A JSON entry in the shape the daemon actually writes.
    fn json_entry(level: &str, msg: &str) -> LogLine {
        LogLine::parse(&format!(
            r#"{{"timestamp":"2026-08-12T14:52:18.024693Z","level":"{level}","fields":{{"message":"{msg}"}},"target":"travsr_daemon"}}"#
        ))
    }

    /// A pre-JSON entry, as still found in rotated files on disk.
    fn text_entry(level: &str, msg: &str) -> LogLine {
        LogLine::parse(&format!(
            "2026-08-12T14:52:18.024693Z  {level} travsr_daemon: {msg}"
        ))
    }

    #[test]
    fn level_filter_keeps_the_requested_severity_and_above() {
        let mut f = LineFilter::new(None, Some(parse_level("warn").unwrap()), None);
        assert!(f.keep(&json_entry("ERROR", "boom")));
        assert!(f.keep(&json_entry("WARN", "careful")));
        assert!(!f.keep(&json_entry("INFO", "routine")));
        assert!(!f.keep(&json_entry("DEBUG", "chatter")));
    }

    /// Rotated files written before the format changed are still on disk and
    /// still the only record of what happened then, so every filter has to work
    /// on both shapes.
    #[test]
    fn the_filters_work_on_pre_json_lines_too() {
        let mut f = LineFilter::new(None, Some(parse_level("warn").unwrap()), None);
        assert!(f.keep(&text_entry("ERROR", "boom")));
        assert!(!f.keep(&text_entry("INFO", "routine")));

        let mut byrepo = LineFilter::new(Some("alpha".to_string()), None, None);
        assert!(byrepo.keep(&LogLine::parse(
            r#"2026-08-12T14:52:18.024693Z  INFO travsr_mcp: served repo="alpha""#
        )));
        assert!(!byrepo.keep(&LogLine::parse(
            r#"2026-08-12T14:52:18.024693Z  INFO travsr_mcp: served repo="beta""#
        )));
    }

    /// The repo tag lives in a named field now, on the event or on the span the
    /// event happened inside, so the filter reads it instead of matching text.
    #[test]
    fn the_repo_filter_reads_the_json_field_on_event_or_span() {
        let mut f = LineFilter::new(Some("alpha".to_string()), None, None);

        assert!(f.keep(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"x","repo":"alpha"},"target":"travsr_daemon"}"#
        )));
        assert!(f.keep(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"served"},"target":"travsr_mcp","span":{"repo":"alpha","name":"tool_call"}}"#
        )));
        // The prefix trap a substring search falls into, now impossible.
        assert!(!f.keep(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"x","repo":"alpha-staging"},"target":"travsr_daemon"}"#
        )));
        assert!(!f.keep(&json_entry("INFO", "no repo at all")));
    }

    /// Rendering: columns, local time, and the date announced once rather than
    /// repeated on all 27 characters of every line.
    #[test]
    fn rendering_puts_the_date_on_a_separator_and_the_rest_in_columns() {
        let mut r = LogRenderer::new(true); // UTC, so the assertion is stable.
        let out = r.render(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"embed_text updated","written":130,"elapsed_ms":34},"target":"travsr_daemon"}"#,
        ));
        assert_eq!(out.len(), 2, "first entry of a day emits its separator");
        assert_eq!(out[0], "── 2026-08-12 UTC ──");
        // Fields after the message are alphabetical, not source order: a
        // `serde_json` object is a sorted map. Deterministic either way, and
        // consistent ordering is what matters when scanning a column of similar
        // entries. Source order would mean enabling `preserve_order`, which
        // changes key ordering for every JSON value in the workspace.
        // INFO renders as a blank label: it is the level of nine lines in ten,
        // so printing it says "nothing unusual" over and over and buries the
        // lines that do say something.
        assert_eq!(
            out[1],
            "14:52:18         daemon       embed_text updated elapsed_ms=34 written=130"
        );

        // Same day: no second separator.
        let again = r.render(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:53:00.000000Z","level":"WARN","fields":{"message":"careful"},"target":"travsr_plugin_host::registry"}"#,
        ));
        assert_eq!(again.len(), 1, "the date is announced once, not per line");
        assert_eq!(
            again[0], "14:53:00  WARN   plugin-host  careful",
            "the target is shortened to the subsystem"
        );

        // A new day announces itself, which is what makes a rotation-spanning
        // read legible.
        let tomorrow = r.render(&LogLine::parse(
            r#"{"timestamp":"2026-08-13T00:00:04.000000Z","level":"INFO","fields":{"message":"daemon starting"},"target":"travsr_daemon"}"#,
        ));
        assert_eq!(tomorrow.len(), 2);
        assert_eq!(tomorrow[0], "── 2026-08-13 UTC ──");
    }

    /// `--json` is for piping into jq or a collector, so every line it emits has
    /// to be an object. It was not: on this repo 142 of 158 lines were pre-JSON
    /// rotations, and `jq` died on the first one with
    /// `parse error: Invalid numeric literal at line 1, column 14`, which makes
    /// the flag useless for the only thing it exists for.
    #[test]
    fn the_json_stream_stays_valid_json_even_where_the_log_is_not() {
        let legacy = LogLine::parse(
            "2026-08-12T13:31:02.217447Z  WARN travsr_plugin_host::registry: #478: rule 4 tripped",
        );
        let wrapped: serde_json::Value = serde_json::from_str(&legacy.to_json_line())
            .expect("a pre-JSON line must still come out as an object");

        assert_eq!(wrapped["timestamp"], "2026-08-12T13:31:02.217447Z");
        assert_eq!(wrapped["level"], "WARN");
        assert_eq!(
            wrapped["target"], "travsr_plugin_host::registry",
            "the trailing colon is not part of the target"
        );
        assert_eq!(
            wrapped["fields"]["message"], "#478: rule 4 tripped",
            "a message containing ': ' must not be cut at it"
        );
        assert_eq!(
            wrapped["unparsed"], true,
            "a consumer has to be able to tell recovered lines from native ones"
        );

        // A continuation frame has no prefix to recover; it still has to be an
        // object rather than raw text.
        let frame = LogLine::parse("    at crates/travsr-daemon/src/lib.rs:512");
        let v: serde_json::Value = serde_json::from_str(&frame.to_json_line()).unwrap();
        assert_eq!(
            v["fields"]["message"], "    at crates/travsr-daemon/src/lib.rs:512",
            "with no prefix to strip, the whole line is the message"
        );

        // Native lines pass through unchanged in content.
        let native = LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"x"},"target":"travsr_daemon"}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&native.to_json_line()).unwrap();
        assert_eq!(v["fields"]["message"], "x");
        assert!(
            v.get("unparsed").is_none(),
            "a native line must not be tagged as recovered"
        );
    }

    /// What the reader already knows is not worth a column. Every repo-scoped
    /// line carried `repo=/Users/me/work/travsr`, thirty-odd characters of a
    /// fact established by the directory the command ran in, and `sock=` spelled
    /// out an absolute path where only the last component differs.
    #[test]
    fn a_single_repo_log_drops_the_repo_it_obviously_belongs_to() {
        let repo = std::path::PathBuf::from("/w/travsr");
        let mut r = LogRenderer::new(true).for_repo(repo);

        let out = r.render(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"control socket bound","repo":"/w/travsr","sock":"/w/travsr/.travsr/daemon-1c0d66a3.sock","transport":"unix"},"target":"travsr_daemon"}"#,
        ));
        let line = out.last().unwrap();
        assert!(
            !line.contains("repo="),
            "the repo the reader is standing in is not news: {line}"
        );
        assert!(
            line.contains("sock=.travsr/daemon-1c0d66a3.sock"),
            "a path under the repo shortens to what varies: {line}"
        );
        assert!(line.contains("transport=unix"), "other fields survive");
    }

    /// In `--global` the repo is the one thing that tells two lines apart, so it
    /// has to stay.
    #[test]
    fn a_global_log_keeps_the_repo_field() {
        let mut r = LogRenderer::new(true);
        let out = r.render(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"INFO","fields":{"message":"served","repo":"/w/travsr"},"target":"travsr_mcp"}"#,
        ));
        assert!(out.last().unwrap().contains("repo=/w/travsr"));
    }

    /// Only the repo's *own* paths are shortened. A path elsewhere on disk is
    /// still the whole answer to where it is.
    #[test]
    fn a_path_outside_the_repo_is_left_whole() {
        let mut r = LogRenderer::new(true).for_repo(std::path::PathBuf::from("/w/travsr"));
        let out = r.render(&LogLine::parse(
            r#"{"timestamp":"2026-08-12T14:52:18.024693Z","level":"WARN","fields":{"message":"tool missing","path":"/opt/homebrew/bin/rust-analyzer"},"target":"travsr_indexer"}"#,
        ));
        let line = out.last().unwrap();
        assert!(
            line.contains("path=/opt/homebrew/bin/rust-analyzer"),
            "{line}"
        );
        assert!(line.contains("WARN"), "a warning keeps its label: {line}");
    }

    /// A torn write is not JSON, and neither is a pre-JSON rotation. Neither may
    /// be dropped: the log is the only record of whatever happened there.
    #[test]
    fn unparseable_and_legacy_lines_are_passed_through_not_dropped() {
        let mut r = LogRenderer::new(true);
        let torn = r.render(&LogLine::parse(r#"{"timestamp":"2026-08-12T14:52:1"#));
        assert_eq!(
            torn,
            vec![r#"{"timestamp":"2026-08-12T14:52:1"#.to_string()]
        );

        let legacy = r.render(&LogLine::parse(
            "2026-08-12T14:52:18.024693Z  INFO travsr_daemon: from before the change",
        ));
        assert!(
            legacy.iter().any(|l| l.contains("from before the change")),
            "a pre-JSON line stays readable: {legacy:?}"
        );
    }

    /// The edge case any line-at-a-time filter gets wrong on the legacy format:
    /// a panic backtrace carries no timestamp and no level, so judging it on its
    /// own merits keeps the frames and drops the ERROR above them, or the
    /// reverse. Either way the reader is left with half an entry. JSON cannot
    /// produce this shape, but rotated files already on disk can.
    #[test]
    fn a_continuation_line_travels_with_the_entry_above_it() {
        let mut f = LineFilter::new(None, Some(parse_level("warn").unwrap()), None);

        assert!(f.keep(&text_entry("ERROR", "phase B panicked")));
        assert!(
            f.keep(&LogLine::parse(
                "    at crates/travsr-daemon/src/lib.rs:512"
            )),
            "a frame under a kept ERROR is part of that entry"
        );
        assert!(
            f.keep(&LogLine::parse("    note: run with RUST_BACKTRACE=full")),
            "and so is the next frame"
        );

        // Now an entry the filter rejects: its continuations go with it.
        assert!(!f.keep(&text_entry("INFO", "routine")));
        assert!(
            !f.keep(&LogLine::parse("    continuation of the routine line")),
            "a frame under a dropped INFO must not survive its parent"
        );
    }

    #[test]
    fn an_unreadable_level_is_shown_rather_than_hidden() {
        // A format change must not silently empty the output.
        let mut f = LineFilter::new(None, Some(parse_level("error").unwrap()), None);
        assert!(f.keep(&LogLine::parse(
            "2026-08-12T14:52:18.024693Z something-unparseable"
        )));
    }

    #[test]
    fn since_drops_entries_older_than_the_cutoff() {
        let mut f = LineFilter::new(None, None, Some(chrono::Duration::hours(1)));
        // Fixed past date: comfortably older than one hour ago, whenever this runs.
        assert!(!f.keep(&LogLine::parse(
            r#"{"timestamp":"2020-01-01T00:00:00.000000Z","level":"INFO","fields":{"message":"ancient"},"target":"travsr_daemon"}"#
        )));
        // A line stamped now is inside the window.
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string();
        assert!(f.keep(&LogLine::parse(&format!(
            r#"{{"timestamp":"{now}","level":"INFO","fields":{{"message":"fresh"}},"target":"travsr_daemon"}}"#
        ))));
    }

    #[test]
    fn entry_starts_are_told_apart_from_continuations() {
        assert!(is_entry_start(
            "2026-08-12T14:52:18.024693Z  INFO travsr_daemon: x"
        ));
        assert!(!is_entry_start("    at src/lib.rs:512"));
        assert!(!is_entry_start(""));
        assert!(!is_entry_start("short"));
    }

    #[test]
    fn bad_level_and_duration_arguments_are_rejected_with_a_usable_message() {
        let e = parse_level("loud").unwrap_err().to_string();
        assert!(e.contains("trace, debug, info, warn or error"), "{e}");

        for bad in ["10x", "abc", "m", ""] {
            assert!(parse_since(bad).is_err(), "`{bad}` must not parse");
        }
        assert_eq!(parse_since("10m").unwrap(), chrono::Duration::minutes(10));
        assert_eq!(parse_since("2h").unwrap(), chrono::Duration::hours(2));
        assert_eq!(parse_since("45s").unwrap(), chrono::Duration::seconds(45));
        assert_eq!(parse_since("1d").unwrap(), chrono::Duration::days(1));
    }

    /// `--version`, the daemon's `version=` telemetry field, and the release
    /// identity must all be the same string. They disagreed before: the daemon
    /// logged the workspace version while `--version` printed the CLI crate's,
    /// and neither carried a prerelease suffix.
    #[test]
    fn release_version_is_reported_consistently() {
        assert!(!RELEASE_VERSION.is_empty(), "version must never be empty");

        // Unset in every local and CI test run, so this asserts the fallback
        // branch. The injected branch is covered by `resolve_version_prefers_the
        // _injected_build_id` below, which calls the real selection function.
        if option_env!("TRAVSR_BUILD_ID").is_none() {
            assert_eq!(
                RELEASE_VERSION,
                env!("CARGO_PKG_VERSION"),
                "an untagged build must report the crate version"
            );
        }

        // What `--version` actually prints, taken from clap rather than assumed.
        let rendered = Cli::command().render_version();
        assert!(
            rendered.contains(RELEASE_VERSION),
            "--version must print the release identity, got {rendered:?}"
        );
    }

    /// Exercises the real `resolve_version`, not a copy of it, so swapping its
    /// arms fails here. The previous version of this test reimplemented the
    /// match locally and asserted against its own copy, which could never have
    /// caught a regression in the production const.
    #[test]
    fn resolve_version_prefers_the_injected_build_id() {
        assert_eq!(
            resolve_version(Some("1.0.0+56c9329"), "1.0.0"),
            "1.0.0+56c9329"
        );
        assert_eq!(resolve_version(None, "1.0.0"), "1.0.0");
    }

    // The stripping that guarantees no channel reaches the baked id lives in
    // `.github/scripts/build-id.sh`, not here, and is covered by that script's
    // `--self-test` (run by CI). A Rust test could only assert against a
    // hand-written string that is already correct, which would pass even if the
    // real stripping regressed.
}
